#![allow(dead_code)]

use crate::driver::test_env::TestEnv;
use anyhow::{bail, Result};
use slog::Logger;
use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::{constants, event::TaskId, subprocess_ipc::SubprocessSender};

use slog::debug;

#[derive(Debug, Clone)]
pub struct GroupContext {
    pub exec_path: PathBuf,
    pub group_dir: PathBuf,
    logger: Logger,
    pub parent_pid: u32,
    pub debug_keepalive: bool,
}

impl GroupContext {
    /// Create a group context based on the group directory path and an optional subprocess task
    /// id.
    ///
    /// XXX: The task id technically should be part of the ProcessContext. However, we need it here
    /// to create the log channel in case we are in a subprocess.
    pub fn new(
        group_dir: PathBuf,
        subproc_info: Option<(TaskId, u32)>,
        debug_keepalive: bool,
    ) -> Result<Self> {
        let task_id = subproc_info.as_ref().map(|t| t.0.clone());
        let parent_pid = subproc_info.map(|t| t.1).unwrap_or_else(std::process::id);
        let socket_path = Self::log_socket_path_(parent_pid);
        let logger = Self::create_logger(socket_path, task_id)?;

        let exec_path = std::env::current_exe().expect("could not acquire parent process path");
        if !exec_path.is_file() {
            bail!("{:?} is not a file.", exec_path)
        }

        // The following should have the effect of "mkdir -p $group_dir"
        fs::create_dir_all(&group_dir)?;

        Ok(Self {
            exec_path,
            group_dir,
            logger,
            parent_pid,
            debug_keepalive,
        })
    }

    pub fn group_dir(&self) -> PathBuf {
        self.group_dir.clone()
    }

    pub fn log_socket_path(&self) -> PathBuf {
        let parent_pid = self.parent_pid;
        PathBuf::from(format!("./log_sock_{parent_pid}"))
    }

    pub fn log_socket_path_(parent_pid: u32) -> PathBuf {
        // group_dir.as_ref().join("log_socket_path")
        // XXX: Here, we have to resort to relative path names, because of API limitations. See
        // also:
        // https://unix.stackexchange.com/questions/367008/why-is-socket-path-length-limited-to-a-hundred-chars
        PathBuf::from(format!("./log_sock_{parent_pid}"))
    }

    pub fn get_root_env(&self) -> Result<TestEnv> {
        let root_env_path = self.group_dir.join(constants::ROOT_ENV_DIR);
        TestEnv::new(root_env_path, self.logger.clone())
    }

    pub fn get_setup_env(&self) -> Result<TestEnv> {
        let setup_path = self.group_dir.join(constants::GROUP_SETUP_DIR);
        TestEnv::new(setup_path, self.logger.clone())
    }

    pub fn get_test_env(&self, test_name: &str) -> Result<TestEnv> {
        let test_path = self.group_dir.join(constants::TESTS_DIR).join(test_name);
        TestEnv::new(test_path, self.logger.clone())
    }

    fn ensure_dir<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = self.group_dir.parent().unwrap().join(path.as_ref());
        if path.is_dir() {
            // println!("GroupContext: directory already exists: {:?}", path);
        } else {
            // println!("GroupContext: creating directory: {:?}", path);
            fs::create_dir_all(path)?;
        }
        Ok(())
    }

    /// Returns the path to the setup artifact directory,
    /// ensuring that the directory actually exists and
    /// contains a copy of the configuration files from root_env
    fn create_setup_dir(&self) -> Result<PathBuf> {
        let root_env_path = self.group_dir.join(constants::ROOT_ENV_DIR);
        let setup_path = self.group_dir.join(constants::GROUP_SETUP_DIR);

        debug!(
            self.logger,
            "Ensuring directory {:?} exists ...", setup_path
        );
        self.ensure_dir(setup_path.clone())?;

        debug!(
            self.logger,
            "Copying configuration from {:?} to {:?} ...", root_env_path, setup_path
        );
        // todo: this function should eventually just `fork` the root environment.
        TestEnv::shell_copy(&root_env_path, &setup_path)?;

        Ok(setup_path)
    }

    /// Returns the path to the setup artifact directory, if it exists.
    fn get_setup_dir(&self) -> Option<PathBuf> {
        let setup_path = self.group_dir.join(constants::GROUP_SETUP_DIR);
        if setup_path.is_dir() {
            Some(setup_path)
        } else {
            None
        }
    }

    /// Returns the path to the artifact directory for this [test_name],
    /// ensuring that the directory actually exists.
    fn create_test_dir(&self, test_name: &str) -> Result<PathBuf> {
        let test_path = self.group_dir.join(constants::TESTS_DIR).join(test_name);
        debug!(self.logger, "Ensuring directory {:?} exists ...", test_path);
        self.ensure_dir(test_path.clone()).map(|_| test_path)
    }

    pub fn create_setup_env(&self) -> Result<TestEnv> {
        let setup_dir = self.create_setup_dir()?;
        TestEnv::new(setup_dir, self.logger.clone())
    }

    pub fn create_test_env(&self, test_name: &str) -> Result<TestEnv> {
        let target_dir = self.create_test_dir(test_name)?;
        if let Some(setup_dir) = self.get_setup_dir() {
            TestEnv::fork_from(
                setup_dir.as_path(),
                target_dir.as_path(),
                self.logger.clone(),
            )
        } else {
            bail!(
                "cannot create TestEnv for {} as setup directory does not exist yet",
                test_name
            )
        }
    }

    pub fn logger(&self) -> Logger {
        self.logger.clone()
    }

    pub fn log(&self) -> &Logger {
        &self.logger
    }

    /// Create a logger for this process.
    fn create_logger(sock_path: PathBuf, subproc_id: Option<TaskId>) -> Result<Logger> {
        if let Some(task_id) = subproc_id {
            let sender = SubprocessSender::new(task_id, sock_path)?;
            let logger = Logger::root(sender, slog::o!());
            Ok(logger)
        } else {
            let logger = super::logger::new_stdout_logger();
            Ok(logger)
        }
    }
}

pub type Command = String;

#[derive(Debug)]
pub struct ProcessContext {
    pub group_context: GroupContext,
    pub constructed_at: SystemTime,
    pub command: Command,
}

impl ProcessContext {
    pub fn new(group_context: GroupContext, command: Command) -> Result<Self> {
        debug!(group_context.log(), "ProcessContext.new");

        let constructed_at = SystemTime::now();

        Ok(Self {
            group_context,
            constructed_at,
            command,
        })
    }

    pub fn logger(&self) -> Logger {
        self.group_context.logger()
    }
}
