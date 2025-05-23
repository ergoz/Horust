use anyhow::{Context, Error, Result};
use libcgroups::common::{
    create_cgroup_manager, CgroupConfig, CgroupManager, ControllerOpt, DEFAULT_CGROUP_ROOT,
};
use nix::sys::signal::Signal;
use nix::unistd;
use oci_spec::runtime::{LinuxCpuBuilder, LinuxMemoryBuilder, LinuxPidsBuilder, LinuxResources};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::{Debug, Formatter};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;
use std::{env, os::fd::RawFd};

use crate::horust::error::{ValidationError, ValidationErrors};

pub fn get_sample_service() -> &'static str {
    include_str!("../../../example_services/sample_service.toml")
}

pub type ServiceName = String;

#[derive(Serialize, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Service {
    #[serde(default)]
    pub name: ServiceName,
    #[serde()]
    pub command: String,
    #[serde(default)]
    pub user: User,
    #[serde(default = "Service::default_working_directory")]
    pub working_directory: PathBuf,
    #[serde(default = "Service::default_stdout_log")]
    pub stdout: LogOutput,
    // todo: provide serialize_with
    #[serde(default, skip_serializing, deserialize_with = "str_to_bytes")]
    pub stdout_rotate_size: u64,
    #[serde(default = "default_as_false")]
    pub stdout_should_append_timestamp_to_filename: bool,
    #[serde(default = "Service::default_stderr_log")]
    pub stderr: LogOutput,
    #[serde(default, with = "humantime_serde")]
    pub start_delay: Duration,
    #[serde(default)]
    pub start_after: Vec<ServiceName>,
    #[serde()]
    pub signal_rewrite: Option<String>,
    #[serde(default)]
    pub restart: Restart,
    #[serde(default)]
    pub healthiness: Healthiness,
    #[serde(default)]
    pub failure: Failure,
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub termination: Termination,
    #[serde(default)]
    pub resource_limit: ResourceLimit,
}

fn default_as_false() -> bool {
    false
}

impl Service {
    fn default_working_directory() -> PathBuf {
        env::current_dir().unwrap()
    }

    fn default_stdout_log() -> LogOutput {
        LogOutput::Stdout
    }

    fn default_stderr_log() -> LogOutput {
        LogOutput::Stderr
    }

    /// Tries to load specific config from path.
    /// Config will be automatically templated from env.
    /// Correct syntax is required for templating to work.
    /// Currently only templating from environment is implemented.
    pub fn from_file<P>(path: &P) -> Result<Self>
    where
        P: AsRef<Path> + ?Sized + AsRef<OsStr> + Debug,
    {
        let preconfig = std::fs::read_to_string(path)?;
        let postconfig = shellexpand::full(&preconfig)?;
        Ok(toml::from_str::<Service>(&postconfig)?)
    }
    /// Creates the environment K=V variables, used for exec into the new process.
    /// User defined environment variables overwrite the predefined values.
    pub fn get_environment(&self) -> Result<Vec<String>> {
        Ok(self.environment.get_environment(
            self.user.clone().get_name()?,
            self.user.clone().get_home()?.display().to_string(),
        ))
    }

    /// Wrapper for single command horust run
    pub fn from_command(command: String) -> Self {
        Service {
            name: command.clone(),
            command,
            ..Default::default()
        }
    }
}

impl Default for Service {
    fn default() -> Self {
        Self {
            name: "".to_owned(),
            start_after: Default::default(),
            working_directory: env::current_dir().unwrap(),
            stdout: Default::default(),
            stdout_rotate_size: 0,
            stdout_should_append_timestamp_to_filename: Default::default(),
            stderr: Default::default(),
            user: Default::default(),
            restart: Default::default(),
            start_delay: Duration::from_secs(0),
            command: "command".to_string(),
            healthiness: Default::default(),
            signal_rewrite: None,
            environment: Default::default(),
            failure: Default::default(),
            termination: Default::default(),
            resource_limit: Default::default(),
        }
    }
}

impl FromStr for Service {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let postconfig = shellexpand::full(s)?.to_string();
        toml::from_str::<Service>(&postconfig).map_err(Error::from)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub enum LogOutput {
    Stderr,
    #[default]
    Stdout,
    Path(PathBuf),
    Pipe(RawFd),
}

impl Serialize for LogOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let as_string: String = self.clone().into();
        serializer.serialize_str(as_string.as_str())
    }
}

impl<'de> Deserialize<'de> for LogOutput {
    fn deserialize<D>(deserializer: D) -> Result<LogOutput, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(LogOutputVisitor)
    }
}

struct LogOutputVisitor;

impl Visitor<'_> for LogOutputVisitor {
    type Value = LogOutput;

    fn expecting(&self, formatter: &mut Formatter) -> std::fmt::Result {
        formatter.write_str("a string with 'STDOUT', 'STDERR', or a full path. All as `String`s ")
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(LogOutput::from(value))
    }
}

impl From<String> for LogOutput {
    fn from(strategy: String) -> Self {
        strategy.as_str().into()
    }
}

impl From<LogOutput> for String {
    fn from(l: LogOutput) -> Self {
        use LogOutput::*;
        match l {
            Stdout => "STDOUT".to_string(),
            Stderr => "STDERR".to_string(),
            Path(path) => {
                let path = path.display();
                path.to_string()
            }
            Pipe(fd) => format!("{fd}"),
        }
    }
}

impl From<&str> for LogOutput {
    fn from(strategy: &str) -> Self {
        match strategy {
            "STDOUT" => LogOutput::Stdout,
            "STDERR" => LogOutput::Stderr,
            path => LogOutput::Path(PathBuf::from(path)),
        }
    }
}

#[derive(Serialize, Clone, Default, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct Environment {
    #[serde(default)]
    pub keep_env: bool,
    #[serde(default)]
    pub re_export: Vec<String>,
    #[serde(default)]
    pub additional: HashMap<String, String>,
}

impl Environment {
    fn get_hostname_val() -> String {
        let hostname_path = "/etc/hostname";
        let localhost = "localhost".to_string();
        if std::path::PathBuf::from(hostname_path).is_file() {
            std::fs::read_to_string(hostname_path).unwrap_or(localhost)
        } else {
            std::env::var("HOSTNAME").unwrap_or(localhost)
        }
    }

    /// Create the environment K=V variables, used for exec into the new process.
    /// User defined environment variables overwrite the predefined variables.
    pub(crate) fn get_environment(&self, user_name: String, user_home: String) -> Vec<String> {
        let mut initial: HashMap<String, String> = self
            .keep_env
            .then(|| std::env::vars().collect())
            .unwrap_or_default();

        let mut additional = self.additional.clone();

        let get_env = |name: &str, default: &str| {
            (
                name.to_string(),
                std::env::var(name).unwrap_or_else(|_| default.to_string()),
            )
        };
        let hostname = ("HOSTNAME".to_string(), Self::get_hostname_val());
        let path_env = get_env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games",
        );
        let user_name = ("USER".to_string(), user_name);
        let user_home = ("HOME".to_string(), user_home);

        let env: HashMap<String, String> = vec![hostname, path_env, user_name, user_home]
            .into_iter()
            .collect();
        // The variables from env have always precedence over initial. E.g. home, and user might differ.
        initial.extend(env);

        // Since I don't know a sane default:
        if let Ok(term) = std::env::var("TERM") {
            initial.entry("TERM".to_string()).or_insert(term);
        }

        let re_export: HashMap<String, String> = self
            .re_export
            .iter()
            .filter_map(|key| {
                std::env::var(key)
                    .map_err(|err| error!("Error getting env key: {}, error: {} ", key, err))
                    .ok()
                    .map(|value| (key.clone(), value))
            })
            .collect();

        // If a variable is re_export, then it has precedence over initial + env.
        initial.extend(re_export);

        // Finally, additional has the higher precedence:
        initial.into_iter().for_each(|(k, v)| {
            additional.entry(k).or_insert(v);
        });

        // This is the suitable format for `exec`
        additional
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect()
    }
}

#[derive(Serialize, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Healthiness {
    pub http_endpoint: Option<String>,
    pub file_path: Option<PathBuf>,
    pub command: Option<String>,
    #[serde(default = "Healthiness::default_max_failed")]
    // todo: use an u32
    pub max_failed: i32,
}

impl Healthiness {
    fn default_max_failed() -> i32 {
        3
    }

    pub(crate) fn has_any_check_defined(&self) -> bool {
        self.http_endpoint.is_some() || self.file_path.is_some() || self.command.is_some()
    }
}

impl Default for Healthiness {
    fn default() -> Self {
        Self {
            http_endpoint: None,
            file_path: None,
            command: None,
            max_failed: 3,
        }
    }
}

/// A user in the system.
/// It can be either a uuid or a username (available in passwd)
#[derive(Serialize, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(untagged)]
pub enum User {
    Uid(u32),
    Name(String),
}

impl From<unistd::Uid> for User {
    fn from(uid: unistd::Uid) -> Self {
        User::Uid(uid.as_raw())
    }
}

impl Default for User {
    fn default() -> Self {
        unistd::getuid().into()
    }
}

impl User {
    pub(crate) fn get_uid(&self) -> Result<unistd::Uid> {
        match &self {
            User::Name(name) => {
                let user = unistd::User::from_name(name)?
                    .with_context(|| format!("User `{}` not found", name))?;
                Ok(user.uid)
            }
            User::Uid(uid) => Ok(unistd::Uid::from_raw(*uid)),
        }
    }

    fn get_raw_user(&self) -> Result<unistd::User> {
        let uid = self.get_uid()?;
        let user =
            unistd::User::from_uid(uid)?.with_context(|| format!("User `{}` not found", uid))?;
        Ok(user)
    }

    fn get_home(&self) -> Result<PathBuf> {
        Ok(self.get_raw_user()?.dir)
    }

    fn get_name(&self) -> Result<String> {
        Ok(self.get_raw_user()?.name)
    }
}

#[derive(Serialize, Clone, Deserialize, Debug, Eq, PartialEq, Hash, Default)]
pub enum ServiceStatus {
    /// The service will be started asap
    Starting,
    /// Service has a pid
    Started,
    /// The service is up and healthy
    Running,
    /// Friendly signal sent, waiting for the process to terminate.
    InKilling,
    /// A successfully exited service.
    Success,
    /// A finished service has done it's job and won't be restarted.
    Finished,
    /// A failed, finished service won't be restarted.
    FinishedFailed,
    /// A Failed service might be restarted if the restart policy demands so.
    Failed,
    /// This is the initial state: A service in Initial state is marked to be runnable:
    /// it will be run as soon as possible.
    #[default]
    Initial,
}

impl std::fmt::Display for ServiceStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.write_str(match self {
            ServiceStatus::Failed => "Failed",
            ServiceStatus::Finished => "Finished",
            ServiceStatus::FinishedFailed => "FinishedFailed",
            ServiceStatus::InKilling => "InKilling",
            ServiceStatus::Initial => "Initial",
            ServiceStatus::Running => "Running",
            ServiceStatus::Started => "Started",
            ServiceStatus::Starting => "Starting",
            ServiceStatus::Success => "Success",
        })
    }
}

#[derive(Serialize, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Restart {
    #[serde(default)]
    pub strategy: RestartStrategy,
    #[serde(default, with = "humantime_serde")]
    pub backoff: Duration,
    #[serde(default = "default_attempts")]
    pub attempts: u32,
}

fn default_attempts() -> u32 {
    0
}

impl Default for Restart {
    fn default() -> Self {
        Restart {
            strategy: Default::default(),
            backoff: Duration::from_secs(0),
            attempts: default_attempts(),
        }
    }
}

#[derive(Serialize, Clone, Deserialize, Default, Debug, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum RestartStrategy {
    Always,
    OnFailure,
    #[default]
    Never,
}

impl From<String> for RestartStrategy {
    fn from(strategy: String) -> Self {
        strategy.as_str().into()
    }
}

impl From<&str> for RestartStrategy {
    fn from(strategy: &str) -> Self {
        match strategy.to_lowercase().as_str() {
            "always" => RestartStrategy::Always,
            "on-failure" => RestartStrategy::OnFailure,
            "never" => RestartStrategy::Never,
            _ => RestartStrategy::Never,
        }
    }
}

#[derive(Serialize, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Failure {
    #[serde(default = "Failure::default_successful_exit_code")]
    pub successful_exit_code: Vec<i32>,
    pub strategy: FailureStrategy,
}

impl Failure {
    fn default_successful_exit_code() -> Vec<i32> {
        vec![0]
    }
}

#[derive(Serialize, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum FailureStrategy {
    Shutdown,
    KillDependents,
    Ignore,
}

impl Default for Failure {
    fn default() -> Self {
        Failure {
            successful_exit_code: Self::default_successful_exit_code(),
            strategy: FailureStrategy::Ignore,
        }
    }
}

impl From<String> for FailureStrategy {
    fn from(strategy: String) -> Self {
        strategy.as_str().into()
    }
}

impl From<&str> for FailureStrategy {
    fn from(strategy: &str) -> Self {
        match strategy.to_lowercase().as_str() {
            "kill-dependents" => FailureStrategy::KillDependents,
            "kill-all" => FailureStrategy::Shutdown,
            "ignore" => FailureStrategy::Ignore,
            _ => FailureStrategy::Ignore,
        }
    }
}

#[derive(Serialize, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Termination {
    #[serde(default)]
    /// Use this signal instead of SIGTERM.
    pub(crate) signal: TerminationSignal,
    #[serde(default = "Termination::default_wait", with = "humantime_serde")]
    /// Time to wait before SIGKILL
    pub wait: Duration,
    #[serde(default = "Vec::new")]
    // Will kill this service if any of the services in Vec are failed
    pub die_if_failed: Vec<ServiceName>,
}

impl Termination {
    fn default_wait() -> Duration {
        Duration::from_secs(5)
    }
}

impl Default for Termination {
    fn default() -> Self {
        Termination {
            signal: Default::default(),
            wait: Self::default_wait(),
            die_if_failed: Vec::new(),
        }
    }
}

#[derive(Serialize, Copy, Clone, Default, Deserialize, Debug, Eq, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
pub enum TerminationSignal {
    HUP,
    INT,
    QUIT,
    ILL,
    TRAP,
    ABRT,
    BUS,
    FPE,
    USR1,
    SEGV,
    USR2,
    PIPE,
    ALRM,
    #[default]
    TERM,
    #[cfg(target_os = "linux")]
    STKFLT,
    CHLD,
    CONT,
    STOP,
    TSTP,
    TTIN,
    TTOU,
    URG,
    XCPU,
    XFSZ,
    VTALRM,
    PROF,
    WINCH,
    IO,
    #[cfg(target_os = "linux")]
    PWR,
    SYS,
}

impl From<TerminationSignal> for Signal {
    fn from(ts: TerminationSignal) -> Self {
        use nix::sys::signal::*;
        match ts {
            TerminationSignal::HUP => SIGHUP,
            TerminationSignal::INT => SIGINT,
            TerminationSignal::QUIT => SIGQUIT,
            TerminationSignal::ILL => SIGILL,
            TerminationSignal::TRAP => SIGTRAP,
            TerminationSignal::ABRT => SIGABRT,
            TerminationSignal::BUS => SIGBUS,
            TerminationSignal::FPE => SIGFPE,
            TerminationSignal::USR1 => SIGUSR1,
            TerminationSignal::SEGV => SIGSEGV,
            TerminationSignal::USR2 => SIGUSR2,
            TerminationSignal::PIPE => SIGPIPE,
            TerminationSignal::ALRM => SIGALRM,
            TerminationSignal::TERM => SIGTERM,
            #[cfg(target_os = "linux")]
            TerminationSignal::STKFLT => SIGSTKFLT,
            TerminationSignal::CHLD => SIGCHLD,
            TerminationSignal::CONT => SIGCONT,
            TerminationSignal::STOP => SIGSTOP,
            TerminationSignal::TSTP => SIGTSTP,
            TerminationSignal::TTIN => SIGTTIN,
            TerminationSignal::TTOU => SIGTTOU,
            TerminationSignal::URG => SIGURG,
            TerminationSignal::XCPU => SIGXCPU,
            TerminationSignal::XFSZ => SIGXFSZ,
            TerminationSignal::VTALRM => SIGVTALRM,
            TerminationSignal::PROF => SIGPROF,
            TerminationSignal::WINCH => SIGWINCH,
            TerminationSignal::IO => SIGIO,
            #[cfg(target_os = "linux")]
            TerminationSignal::PWR => SIGPWR,
            TerminationSignal::SYS => SIGSYS,
        }
    }
}

#[derive(Serialize, Clone, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ResourceLimit {
    #[serde(default)]
    /// The CPU time that the process can use
    pub(crate) cpu: Option<f64>,
    #[serde(default, skip_serializing, deserialize_with = "str_to_optional_bytes")]
    /// The maximum amount of memory that the process can use
    pub(crate) memory: Option<u64>,
    #[serde(default)]
    /// The maximum number of processes/threads that the process can create
    pub(crate) pids_max: Option<u64>,
}

impl ResourceLimit {
    fn has_no_limit(&self) -> bool {
        self.cpu.is_none() && self.memory.is_none() && self.pids_max.is_none()
    }
}

impl Default for ResourceLimit {
    fn default() -> Self {
        ResourceLimit {
            cpu: None,
            memory: None,
            pids_max: None,
        }
    }
}

impl Eq for ResourceLimit {}

impl ResourceLimit {
    pub(crate) fn apply(&self, name: &str, pid: unistd::Pid) -> anyhow::Result<()> {
        if self.has_no_limit() {
            return Ok(());
        }

        // has to be an absolute path for cgroups v2
        let cgroup_path = Path::new(DEFAULT_CGROUP_ROOT).join(format!("horust_{}", name));
        let manager = create_cgroup_manager(CgroupConfig {
            cgroup_path: cgroup_path.to_path_buf(),
            systemd_cgroup: false,
            container_name: name.to_string(),
        })
        .with_context(|| format!("Failed to create cgroup manager for {}", name))?;
        let mut resource = LinuxResources::default();
        if let Some(cpu) = self.cpu {
            let cpu = LinuxCpuBuilder::default()
                .period(100_000u64)
                .quota((cpu * 100_000.0) as i64)
                .build()?;
            resource.set_cpu(Some(cpu));
        }
        if let Some(mem) = self.memory {
            let memory = LinuxMemoryBuilder::default().limit(mem as i64).build()?;
            resource.set_memory(Some(memory));
        }
        if let Some(pid_max) = self.pids_max {
            let pid = LinuxPidsBuilder::default().limit(pid_max as i64).build()?;
            resource.set_pids(Some(pid));
        }

        manager
            .add_task(pid)
            .with_context(|| format!("Failed to add task to cgroup {}", name))?;
        manager
            .apply(&ControllerOpt {
                resources: &resource,
                disable_oom_killer: false,
                oom_score_adj: None,
                freezer_state: None,
            })
            .with_context(|| format!("Failed to apply resource limits to cgroup {}", name))?;

        Ok(())
    }
}

/// Runs some validation checks on the services.
/// TODO: if redirect output is file, check it exists and permissions.
pub fn validate(services: Vec<Service>) -> Result<Vec<Service>, ValidationErrors> {
    let mut errors = vec![];
    services.iter().for_each(|service| {
        if service.command.is_empty() {
            errors.push(ValidationError::CommandEmpty {
                service: service.name.clone(),
            });
        }
        if !service.start_after.is_empty() {
            debug!(
                "Checking if all dependencies of '{}' exists, deps: {:?}",
                service.name, service.start_after
            );
        }
        service.start_after.iter().for_each(|name| {
            let passed = services.iter().any(|s| s.name == *name);
            if !passed {
                errors.push(ValidationError::MissingDependency {
                    before: name.into(),
                    after: service.name.clone(),
                });
            }
        });
    });
    if errors.is_empty() {
        Ok(services)
    } else {
        Err(ValidationErrors::new(errors))
    }
}

fn str_to_bytes<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    bytefmt::parse(s).map_err(de::Error::custom)
}

fn str_to_optional_bytes<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = Option::<String>::deserialize(deserializer)?;
    match s {
        Some(s) => bytefmt::parse(s).map(Some).map_err(de::Error::custom),
        None => Ok(None),
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;
    use std::time::Duration;

    use crate::horust::formats::ResourceLimit;
    use crate::horust::formats::{
        validate, Environment, Failure, FailureStrategy, Healthiness, Restart, RestartStrategy,
        Service, Termination, TerminationSignal::TERM,
    };
    use crate::horust::get_sample_service;

    impl Service {
        pub fn start_after(name: &str, start_after: Vec<&str>) -> Self {
            Self {
                name: name.to_owned(),
                start_after: start_after.into_iter().map(|v| v.into()).collect(),
                ..Default::default()
            }
        }

        pub fn from_name(name: &str) -> Self {
            Self::start_after(name, Vec::new())
        }
    }

    #[test]
    fn test_should_correctly_deserialize_sample() {
        let current_user_name: String = super::User::default().get_name().unwrap();
        let expected = Service {
            name: "".to_string(),
            command: "/bin/bash -c \'echo hello world\'".to_string(),
            user: super::User::Name(current_user_name),
            environment: Environment {
                keep_env: false,
                re_export: vec!["PATH".to_string(), "DB_PASS".to_string()],
                additional: vec![("key".to_string(), "value".to_string())]
                    .into_iter()
                    .collect(),
            },
            working_directory: "/tmp/".into(),
            stdout: "/var/logs/hello_world_svc/stdout.log".into(),
            stdout_rotate_size: 100_000_000,
            stdout_should_append_timestamp_to_filename: false,
            stderr: "STDERR".into(),
            start_delay: Duration::from_secs(2),
            start_after: vec!["database".into(), "backend.toml".into()],
            restart: Restart {
                strategy: RestartStrategy::Never,
                backoff: Duration::from_millis(0),
                attempts: 0,
            },
            healthiness: Healthiness {
                http_endpoint: Some("http://localhost:8080/healthcheck".into()),
                file_path: Some("/var/myservice/up".into()),
                command: Some("curl -s http://localhost:8080/healthcheck".into()),
                ..Default::default()
            },
            signal_rewrite: None,
            failure: Failure {
                successful_exit_code: vec![0, 1, 255],
                strategy: FailureStrategy::Ignore,
            },
            termination: Termination {
                signal: TERM,
                wait: Duration::from_secs(10),
                die_if_failed: vec!["db.toml".into()],
            },
            resource_limit: ResourceLimit {
                cpu: Some(0.5),
                memory: Some(100 * 1024 * 1024),
                pids_max: Some(100),
            },
        };

        let service =
            Service::from_str(get_sample_service()).expect("error on deserializing the manifest");
        assert_eq!(expected, service);
    }

    #[test]
    fn test_should_fail_on_not_existing_envvar() {
        let cfg = r#"command = "/bin/bash -c 'echo hello world'"
start-delay = "2s"
start-after = ["another.toml", "second.toml"]
stdout = "STDOUT"
stderr = "/var/logs/hello_world_svc/stderr.log"
user = "$SOMETHING"
working-directory = "/tmp/"
"#
        .to_string();
        assert!(Service::from_str(&cfg).is_err());
    }

    #[test]
    fn test_expansion_ok_without_env() {
        let cfg = r#"command = "/bin/bash -c 'echo hello world'"
start-delay = "2s"
start-after = ["another.toml", "second.toml"]
stdout = "STDOUT"
stderr = "/var/logs/hello_world_svc/stderr.log"
user = "SOMETHING"
working-directory = "/tmp/"
"#
        .to_string();
        assert!(Service::from_str(&cfg).is_ok());
    }

    #[test]
    fn test_validate() {
        // Service does not exists:
        let services = vec![Service::start_after("a", vec!["b"])];
        validate(services).unwrap_err();

        // Command is empty:
        let services = vec![Service::from_command("".into())];
        validate(services).unwrap_err();

        // Should pass validation:
        let services = vec![
            Service::from_name("b"),
            Service::start_after("a", vec!["b"]),
        ];
        validate(services).expect("Validation failed");
    }
}
