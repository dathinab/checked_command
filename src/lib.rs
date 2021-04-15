//! A `std::process::Command` replacement which is a bit more flexible and testable.
//!
//! For now this is focused on cases which wait until the subprocess is completed
//! and then map the output (or do not care about the output).
//!
//! - by default check the exit status
//!
//! - bundle a mapping of the captured stdout/stderr to an result into the command,
//!   i.e. the `Command` type is `Command<Output, Error>` e.g. `Command<Vec<String>, Error>`.
//!
//! - implicitly define if stdout/stderr needs to be captured to prevent mistakes
//!   wrt. this, this is done through through the same mechanism which is used to
//!   define how the output is mapped, e.g. `Command::new("ls", ReturnStdoutString)`
//!   will implicitly enabled stdout capturing and disable `stderr` capturing.
//!
//! - allow replacing command execution with an callback, this is mainly used to
//!   allow mocking the command.
//!
//! - besides allowing to decide weather the sub-process should inherit the environment and
//!   which variables get removed/set/overwritten this type also allows you to whitelist which
//!   env variables should be inherited.
//!
//! - do not have `&mut self` pass through based API. This makes it more bothersome to create
//!   functions which create and return commands, which this types intents to make simple so
//!   that you can e.g. have a function like `fn ls_command() -> Command<Vec<String>, Error>`
//!   which returns a command which if run runs the ls command and returns a vector of string
//!   (or an error if spawning, running or utf8 validation fails).
//!
//! - be generic over Output and Error type but dynamic over how the captured stdout/err is
//!   mapped to the given `Result<Output, Error>`. This allows you to e.g. at runtime switch
//!   between different function which create a command with the same output but on different
//!   ways (i.e. with different called programs and output mapping, e.g. based on a config
//!   setting).
//!
//! # Basic Examples
//!
//! ```rust
//! use mapped_command::{Command, ExecResult, output_mapping::{MapStdoutString, CommandExecutionWithStringOutputError as Error, ReturnStdoutString,}};
//!
//! /// Usage: `echo().run()`.
//! fn echo() -> Command<String, Error> {
//!     // implicitly enables stdout capturing but not stderr capturing
//!     // and converts the captured bytes to string
//!     Command::new("echo", ReturnStdoutString)
//! }
//!
//! /// Usage: `ls_command().run()`.
//! fn ls_command() -> Command<Vec<String>, Error> {
//!     Command::new("ls", MapStdoutString(|out| {
//!         let lines = out.lines().map(Into::into).collect::<Vec<_>>();
//!         Ok(lines)
//!     }))
//! }
//!
//! fn main() {
//!     let res = ls_command()
//!         //mock
//!         .with_mock_result(|_options, capture_stdout, capture_stderr| {
//!             assert_eq!(capture_stdout, true);
//!             assert_eq!(capture_stderr, false);
//!             Ok(ExecResult {
//!                 exit_status: 0.into(),
//!                 // Some indicates in the mock that stdout was captured, None would mean it was not.
//!                 stdout: Some("foo\nbar\ndoor\n".to_owned().into()),
//!                 ..Default::default()
//!             })
//!         })
//!         // run, check exit status and map captured outputs
//!         .run()
//!         .unwrap();
//!
//!     assert_eq!(res, vec!["foo", "bar", "door"]);
//!
//!     let err = ls_command()
//!         //mock
//!         .with_mock_result(|_options, capture_stdout, capture_stderr| {
//!             assert_eq!(capture_stdout, true);
//!             assert_eq!(capture_stderr, false);
//!             Ok(ExecResult {
//!                 exit_status: 1.into(),
//!                 stdout: Some("foo\nbar\ndoor\n".to_owned().into()),
//!                 ..Default::default()
//!             })
//!         })
//!         .run()
//!         .unwrap_err();
//!
//!     assert_eq!(err.to_string(), "Unexpected exit status. Got: 0x1, Expected: 0x0");
//! }
//! ```
//!
//! # Handling arguments and environment variables
//!
//! ```rust
//! use mapped_command::{Command, output_mapping::ReturnStdoutString, env::EnvUpdate};
//! # #[cfg(unix)]
//! # fn main() {
//! std::env::set_var("FOOBAR", "the foo");
//! std::env::set_var("DODO", "no no");
//! let echoed = Command::new("bash", ReturnStdoutString)
//!     .with_arguments(&["-c", "echo $0 ${DODO:-yo} $FOOBAR $BARFOOT $(pwd)", "arg1"])
//!     .with_inherit_env(false)
//!     .with_env_update("BARFOOT", "the bar")
//!     //inherit this even if env inheritance is disabled (it is see above)
//!     .with_env_update("FOOBAR", EnvUpdate::Inherit)
//!     .with_working_directory_override(Some("/usr"))
//!     .run()
//!     .unwrap();
//!
//! assert_eq!(echoed, "arg1 yo the foo the bar /usr\n");
//! # }
//! ```
//!
use std::{
    ffi::OsString,
    fmt::Debug,
    io,
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::Arc,
};

use pipe::PipeSetup;
use thiserror::Error;

use crate::{
    env::EnvUpdate,
    pipe::{ProcessInput, ProcessOutput},
    spawn::{ChildHandle, SpawnOptions, Spawner},
    utils::NoDebug,
};

pub use self::exit_status::*;

#[macro_use]
mod utils;
pub mod env;
mod exit_status;
pub mod mock;
pub mod output_mapping;
pub mod pipe;
pub mod spawn;
pub mod sys;

/// A collection of imports from `mapped_command` which are commonly used.
///
/// This includes **all** provided output mappings.
pub mod prelude {
    pub use crate::{
        env::EnvUpdate,
        output_mapping::*,
        pipe::{PipeSetup, ProcessInput, ProcessOutput},
        Child, Command,
    };
}

/// A alternative to `std::process::Command` see module level documentation.
pub struct Command<Output, Error>
where
    Output: 'static,
    Error: From<io::Error> + From<UnexpectedExitStatus> + 'static,
{
    spawn_options: SpawnOptions,
    expected_exit_status: Option<ExitStatus>,
    output_mapping: NoDebug<Box<dyn OutputMapping<Output = Output, Error = Error>>>,
    spawn_impl: NoDebug<Arc<dyn Spawner>>,
}

impl<Output, Error> Command<Output, Error>
where
    Output: 'static,
    Error: From<io::Error> + From<UnexpectedExitStatus> + 'static,
{
    /// Create a new command for given program and output mapping.
    ///
    /// The output mapping will imply if stdout/stderr is captured and how the
    /// captured output is mapped to a `Result<Self::Output, Self::Error>`.
    ///
    pub fn new(
        program: impl Into<OsString>,
        output_mapping: impl OutputMapping<Output = Output, Error = Error>,
    ) -> Self {
        Command {
            spawn_options: SpawnOptions::new(program.into()),
            expected_exit_status: Some(ExitStatus::Code(0)),
            output_mapping: NoDebug(Box::new(output_mapping) as _),
            spawn_impl: NoDebug(sys::default_spawner_impl()),
        }
    }

    /// Returns this command with new arguments added to the end of the argument list
    pub fn with_arguments<T>(mut self, args: impl IntoIterator<Item = T>) -> Self
    where
        T: Into<OsString>,
    {
        self.arguments.extend(args.into_iter().map(|v| v.into()));
        self
    }

    /// Returns this command with a new argument added to the end of the argument list
    pub fn with_argument(mut self, arg: impl Into<OsString>) -> Self {
        self.arguments.push(arg.into());
        self
    }

    /// Returns this command with the map of env updates updated by given iterator of key value pairs.
    ///
    /// If any key from the new map already did exist in the current updates it will
    /// replace the old key & value.
    ///
    /// - Common supported values for keys include `OsString`, `&OsStr`, `String`, `&str`.
    /// - Common supported values for values include `EnvUpdate`, `OsString`, `&OsStr`, `String`,
    ///   `&str`
    ///
    /// So you can pass in containers like `Vec<(&str, &str)>`, `HashMap<&str, &str>` or
    /// `HashMap<OsString, EnvUpdate>`, etc.
    ///
    /// # Warning
    ///
    /// The keys of env variables will *not* be evaluated for syntactic validity.
    /// Setting a key invalid on given platform *might* cause the process spawning to
    /// fail (e.g. using a key lik `"="` or `""`). It also *might* also do other thinks
    /// like the env variable being passed in but being unaccessible or similar. It's completely
    /// dependent on the OS and the impl. of `std::process::Command` or whatever is used to
    /// execute the command.
    pub fn with_env_updates<K, V>(mut self, map: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<EnvUpdate>,
    {
        self.env_builder
            .extend(map.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Returns this command with the map of env updates updated by one key value pair.
    ///
    /// If the new key already did exist in the current updates it will replace that
    /// old key & value.
    ///
    /// See [`Command::with_env_updates()`].
    pub fn with_env_update(
        mut self,
        key: impl Into<OsString>,
        value: impl Into<EnvUpdate>,
    ) -> Self {
        self.env_builder.insert_update(key.into(), value.into());
        self
    }

    /// Returns this command with a change to weather or the sub-process will inherit env variables.
    ///
    /// See [`Command::inherit_env()`] for how this affects the sub-process env.
    pub fn with_inherit_env(mut self, do_inherit: bool) -> Self {
        self.env_builder.set_inherit_env(do_inherit);
        self
    }

    /// Replaces the working directory override.
    ///
    /// Setting it to `None` will unset the override making the spawned
    /// process inherit the working directory from the spawning process.
    pub fn with_working_directory_override(
        mut self,
        wd_override: Option<impl Into<PathBuf>>,
    ) -> Self {
        self.working_directory_override = wd_override.map(Into::into);
        self
    }

    /// Set which exit status is treated as successful.
    ///
    /// **This enables exit status checking even if it
    ///   was turned of before.**
    pub fn with_expected_exit_status(mut self, exit_status: impl Into<ExitStatus>) -> Self {
        self.expected_exit_status = Some(exit_status.into());
        self
    }

    /// Disables exit status checking.
    pub fn without_expected_exit_status(mut self) -> Self {
        self.expected_exit_status = None;
        self
    }

    /// Sets a custom stdout pipe setup, this is ignored if [`OutputMapping::needs_captured_stdout()`] is `true`.
    ///
    /// See [`SpawnOptions::custom_stdout_setup`].
    pub fn with_custom_stdout_setup(mut self, pipe_setup: impl Into<PipeSetup>) -> Self {
        self.custom_stdout_setup = Some(pipe_setup.into());
        self
    }

    /// Removes any previously set custom stdout setup
    pub fn without_custom_stdout_setup(mut self) -> Self {
        self.custom_stdout_setup = None;
        self
    }

    /// Sets a custom stderr pipe setup, this is ignored if [`OutputMapping::needs_captured_stderr()`] is `true`.
    ///
    /// See [`SpawnOptions::custom_stderr_setup`].
    pub fn with_custom_stderr_setup(mut self, pipe_setup: impl Into<PipeSetup>) -> Self {
        self.custom_stderr_setup = Some(pipe_setup.into());
        self
    }

    /// Removes any previously set custom stderr setup
    pub fn without_custom_stderr_setup(mut self) -> Self {
        self.custom_stderr_setup = None;
        self
    }

    /// Sets the custom stdin pipe setup.
    pub fn with_custom_stdin_setup(mut self, pipe_setup: impl Into<PipeSetup>) -> Self {
        self.custom_stdin_setup = Some(pipe_setup.into());
        self
    }

    /// Removes any previously set custom stdin setup
    pub fn without_custom_stdin_setup(mut self) -> Self {
        self.custom_stdin_setup = None;
        self
    }

    /// Runs the command. Basically `self.spawn()?.wait()`.
    ///
    /// See [`Command::spawn()`] and [`Child::wait()`] for more details.
    pub fn run(self) -> Result<Output, Error> {
        self.spawn()?.wait()
    }

    /// Spawns a new child process based on this command type.
    ///
    /// Use [`Child::wait()`] to await any results.
    ///
    /// Internally spawning the actual child process is delegated to the [`Spawner`] instance
    /// which by default spawns a child process but this can be changed by setting a different
    /// spawn implementation using [`Command::with_spawn_impl()`] which e.g. [`Command::with_mock_result()`]
    /// uses internally.
    ///
    /// The [`Spawner`] is responsible to make sure all contained [`SpawnOptions`] are applied appropriately,
    /// i.e. the right program is launched with the right arguments and the right environment and working
    /// directory as well as the right setup of pipes for stdout, stderr and stdin where needed.
    ///
    /// If the used output mapping requires stdout/stderr to be captured this will be setup appropriately,
    /// furthermore the pipes used for capturing can *not* be extracted using [`Child::take_stdout()`] (same
    /// for err). Only by using a [`OutputMapping`] which doesn't use the specific pipe for capturing
    /// and using [`Command::with_custom_stdout_setup()`] can you extract a stdout pipe between spawning
    /// the child and awaiting it's completion.
    ///
    /// You can always use [`Command::with_custom_stdin_setup()`] to setup a pipe for stdin and then
    /// extract it and write to the subprocess input.
    ///
    /// *You should be warned that by **default** the `stdout`/`stderr` pipes are only read one [`Child::wait()`] is called*.
    /// In some situations when combined with a piped `stdin` and the buffers being full this can lead to a quasi dead lock
    /// and hang execution. *This is not specific to this library but a property of more of less any OS!*.
    ///
    /// It is possible to provide a spawn implementation which uses a thread to capture stdout and err in which
    /// case it isn't a problem. (TODO: In the future this will likely be supported directly through an
    /// flag in [`SpawnOptions`])
    ///
    /// # Error
    ///
    /// Spawning a process can fail with an `io::Error`.
    ///
    /// # Panics
    ///
    /// Due to limitations of rusts standard library using invalid names for environment variables
    /// can under some circumstances lead to panics (and others to io::Errors while spawning).
    ///
    /// TODO: While this problem is rooted in rust std future versions might work around it.
    ///
    /// Note: Not using `EnvUpdate::Inherit` (with `inherit_env() == false`) will largely decrease
    /// (TODO or eliminate?) the chance for such a panic to appear.
    pub fn spawn(self) -> Result<Child<Output, Error>, io::Error> {
        let Command {
            spawn_options,
            output_mapping,
            spawn_impl,
            expected_exit_status,
        } = self;

        let child = spawn_impl.spawn(
            spawn_options,
            output_mapping.needs_captured_stdout(),
            output_mapping.needs_captured_stderr(),
        )?;

        Ok(Child {
            child: NoDebug(child),
            output_mapping,
            expected_exit_status,
        })
    }

    /// Replaces the default spawn implementation.
    ///
    /// This is used by [`Command::with_mock_result()`] and
    /// similar.
    ///
    /// Besides mocking this can also be used to argument the
    /// spawning of an process, e.g. by logging or different
    /// handling of malformed environment variable names.
    pub fn with_spawn_impl(mut self, spawn_impl: Arc<dyn Spawner>) -> Self {
        self.spawn_impl = NoDebug(spawn_impl);
        self
    }

    /// Syntax short form for `.with_spawn_impl(crate::mock::mock_result(func))`
    pub fn with_mock_result(
        self,
        func: impl 'static + Send + Sync + Fn(SpawnOptions, bool, bool) -> Result<ExecResult, io::Error>,
    ) -> Self {
        self.with_spawn_impl(mock::mock_result(func))
    }

    /// Syntax short form for `.with_spawn_impl(crate::mock::mock_result_once(func))`
    pub fn with_mock_result_once(
        self,
        func: impl 'static + Send + FnOnce(SpawnOptions, bool, bool) -> Result<ExecResult, io::Error>,
    ) -> Self {
        self.with_spawn_impl(mock::mock_result_once(func))
    }

    /// Returns true if [`OutputMapping::needs_captured_stdout()`] returns true.
    pub fn will_capture_stdout(&self) -> bool {
        self.output_mapping.needs_captured_stdout()
    }

    /// Returns true if [`OutputMapping::needs_captured_stderr()`] returns true.
    pub fn will_capture_stderr(&self) -> bool {
        self.output_mapping.needs_captured_stderr()
    }
}

impl<Output, Error> Debug for Command<Output, Error>
where
    Output: 'static,
    Error: From<io::Error> + From<UnexpectedExitStatus> + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Command {
            spawn_options,
            spawn_impl,
            expected_exit_status,
            output_mapping,
        } = self;
        f.debug_struct("Command")
            .field("expected_exit_status", expected_exit_status)
            .field("output_mapping", output_mapping)
            .field("spawn_options", spawn_options)
            .field("spawn_impl", spawn_impl)
            .finish()
    }
}

impl<Output, Error> Deref for Command<Output, Error>
where
    Output: 'static,
    Error: From<io::Error> + From<UnexpectedExitStatus> + 'static,
{
    type Target = SpawnOptions;

    fn deref(&self) -> &Self::Target {
        &self.spawn_options
    }
}

impl<Output, Error> DerefMut for Command<Output, Error>
where
    Output: 'static,
    Error: From<io::Error> + From<UnexpectedExitStatus> + 'static,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.spawn_options
    }
}

/// Trait used to configure what [`Command::run()`] returns.
pub trait OutputMapping: 'static {
    /// The output produced by this command, if it is run and doesn't fail.
    type Output: 'static;

    /// The error produced by this command, if it is run and does fail.
    type Error: 'static;

    /// Return if stdout needs to be captured for this output mapping `map_output` function.
    ///
    /// *This should be a pure function only depending on `&self`.*
    ///
    /// This is called when creating the command, storing the result of it
    /// in the command settings.
    fn needs_captured_stdout(&self) -> bool;

    /// Return if stderr needs to be captured for this output mapping `map_output` function.
    ///
    /// *This should be a pure function only depending on `&self`.*
    ///
    /// This is called when creating the command, storing the result of it
    /// in the command settings.
    fn needs_captured_stderr(&self) -> bool;

    /// The function called once the command's run completed.
    ///
    /// This function is used to convert the captured stdout/stderr
    /// to an instance of the given `Output` type.
    ///
    /// If exist code checking is enabled and fails this function will
    /// not be called.
    ///
    /// If it is disabled this function will be called and the implementation
    /// can still decide to fail due to an unexpected/bad exit status.
    fn map_output(self: Box<Self>, result: ExecResult) -> Result<Self::Output, Self::Error>;
}

/// Child Process (Handle).
pub struct Child<Output, Error>
where
    Output: 'static,
    Error: From<io::Error> + From<UnexpectedExitStatus> + 'static,
{
    expected_exit_status: Option<ExitStatus>,
    output_mapping: NoDebug<Box<dyn OutputMapping<Output = Output, Error = Error>>>,
    child: NoDebug<Box<dyn ChildHandle>>,
}

//FIXME: Use non std proved Debug derive which better handles the bounds
impl<Output, Error> Debug for Child<Output, Error>
where
    Output: 'static,
    Error: From<io::Error> + From<UnexpectedExitStatus> + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Child {
            expected_exit_status,
            output_mapping,
            child,
        } = self;
        f.debug_struct("Child")
            .field("expected_exit_status", expected_exit_status)
            .field("output_mapping", output_mapping)
            .field("child", child)
            .finish()
    }
}

impl<Output, Error> Child<Output, Error>
where
    Output: 'static,
    Error: From<io::Error> + From<UnexpectedExitStatus> + 'static,
{
    /// Awaits the exit of the child mapping the captured output.
    ///
    /// Depending of the setup this either does start capturing the
    /// output (default) which needs to be captured or just waits until the
    /// output is successfully captured and the process exited.
    ///
    /// See [`Command::spawn()`] about how this combined with stdin usage
    /// can potentially lead to problems.
    ///
    pub fn wait(self) -> Result<Output, Error> {
        let Child {
            child,
            output_mapping,
            expected_exit_status,
        } = self;

        let result = child.0.wait_with_output()?;

        if let Some(status) = expected_exit_status {
            if status != result.exit_status {
                return Err(UnexpectedExitStatus {
                    got: result.exit_status,
                    expected: status,
                }
                .into());
            }
        }

        output_mapping.0.map_output(result)
    }

    /// Takes out any "left over" stdout pipe.
    ///
    /// See [`SpawnOptions::custom_stdout_setup`].
    pub fn take_stdout(&mut self) -> Option<Box<dyn ProcessOutput>> {
        self.child.take_stdout()
    }

    /// Takes out any "left over" stderr pipe.
    ///
    /// See [`SpawnOptions::custom_stdout_setup`].
    pub fn take_stderr(&mut self) -> Option<Box<dyn ProcessOutput>> {
        self.child.take_stderr()
    }

    /// Takes out the stdin pipe, if one was setup.
    pub fn take_stdin(&mut self) -> Option<Box<dyn ProcessInput>> {
        self.child.take_stdin()
    }
}

/// The command failed due to an unexpected exit status.
///
/// By default this means the exit status was not 0, but
/// this can be reconfigured.
#[derive(Debug, Error)]
#[error("Unexpected exit status. Got: {got}, Expected: {expected}")]
pub struct UnexpectedExitStatus {
    pub got: ExitStatus,
    pub expected: ExitStatus,
}

/// Type used for `exec_replacement_callback` to return mocked output and exit status.
#[derive(Debug, Default)]
pub struct ExecResult {
    /// The exit status the process did exit with.
    pub exit_status: ExitStatus,

    /// The stdout output captured during sub-process execution (if any).
    ///
    /// This must be `Some` if `stdout` is expected to be captured, it must
    /// be `None` if it's expected to not be captured.
    pub stdout: Option<Vec<u8>>,

    /// The stderr output captured during sub-process execution (if any).
    ///
    /// This must be `Some` if `stderr` is expected to be captured, it must
    /// be `None` if it's expected to not be captured.
    pub stderr: Option<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use thiserror::Error;

    #[derive(Debug, Error)]
    enum TestCommandError {
        #[error(transparent)]
        Io(#[from] io::Error),

        #[error(transparent)]
        UnexpectedExitStatus(#[from] UnexpectedExitStatus),

        #[error("TestCase error: {0}")]
        Prop(TestCaseError),
    }

    impl From<TestCaseError> for TestCommandError {
        fn from(prop_err: TestCaseError) -> Self {
            Self::Prop(prop_err)
        }
    }

    impl TestCommandError {
        pub fn unwrap_prop(self) -> TestCaseError {
            match self {
                Self::Io(err) => panic!("unexpected io error: {:?}", err),
                Self::UnexpectedExitStatus(err) => panic!("unexpected exit status: {:?}", err),
                Self::Prop(prop_err) => return prop_err,
            }
        }
    }

    struct TestOutputMapping {
        capture_stdout: bool,
        capture_stderr: bool,
    }

    impl OutputMapping for TestOutputMapping {
        type Output = bool;
        type Error = TestCommandError;

        fn needs_captured_stdout(&self) -> bool {
            self.capture_stdout
        }
        fn needs_captured_stderr(&self) -> bool {
            self.capture_stderr
        }

        fn map_output(self: Box<Self>, result: ExecResult) -> Result<Self::Output, Self::Error> {
            (|| {
                prop_assert_eq!(result.stdout.is_some(), self.needs_captured_stdout());
                prop_assert_eq!(result.stderr.is_some(), self.needs_captured_stderr());
                Ok(())
            })()?;
            Ok(true)
        }
    }

    mod Command {
        #![allow(non_snake_case)]

        mod new {
            use crate::output_mapping::*;
            use std::ffi::OsStr;

            use super::super::super::*;
            use proptest::prelude::*;

            #[test]
            fn comp_can_be_created_using_str_string_os_str_or_os_string() {
                Command::new("ls", ReturnNothing);
                Command::new("ls".to_owned(), ReturnNothing);
                Command::new(OsString::from("ls"), ReturnNothing);
                Command::new(OsStr::new("ls"), ReturnNothing);
            }

            #[test]
            fn comp_when_creating_command_different_capture_modes_can_be_used() {
                Command::new("foo", ReturnNothing);
                Command::new("foo", ReturnStdout);
                Command::new("foo", ReturnStderr);
                Command::new("foo", ReturnStdoutAndErr);
            }

            proptest! {
                #[test]
                fn the_used_program_can_be_queried(s in any::<OsString>()) {
                    let s = OsStr::new(&*s);
                    let cmd = Command::new(s, ReturnNothing);
                    prop_assert_eq!(&*cmd.program, s)
                }
            }
        }

        mod arguments {
            use super::super::super::*;
            use crate::output_mapping::*;
            use proptest::prelude::*;
            use std::{collections::HashSet, ffi::OsStr, iter};

            #[test]
            fn default_arguments_are_empty() {
                let cmd = Command::new("foo", ReturnNothing);
                assert!(cmd.arguments.is_empty());
            }

            #[test]
            fn comp_arguments_can_be_set_from_iterables() {
                Command::new("foo", ReturnNothing).with_arguments(Vec::<OsString>::new());
                Command::new("foo", ReturnNothing).with_arguments(HashSet::<OsString>::new());
                Command::new("foo", ReturnNothing).with_arguments(&[] as &[OsString]);
            }

            proptest! {
                #[test]
                fn new_arguments_can_be_added(
                    cmd in any::<OsString>(),
                    argument in any::<OsString>(),
                    arguments in proptest::collection::vec(any::<OsString>(), 0..5),
                    arguments2 in proptest::collection::vec(any::<OsString>(), 0..5)
                ) {
                    let cmd = OsStr::new(&*cmd);
                    let cmd = Command::new(cmd, ReturnNothing)
                        .with_arguments(&arguments);
                    prop_assert_eq!(&cmd.arguments, &arguments);
                    let cmd = cmd.with_argument(&argument);
                    prop_assert_eq!(
                        cmd.arguments.iter().collect::<Vec<_>>(),
                        arguments.iter().chain(iter::once(&argument)).collect::<Vec<_>>()
                    );
                    let cmd = cmd.with_arguments(&arguments2);
                    prop_assert_eq!(
                        cmd.arguments.iter().collect::<Vec<_>>(),
                        arguments.iter()
                            .chain(iter::once(&argument))
                            .chain(arguments2.iter())
                            .collect::<Vec<_>>()
                    );
                }
            }
        }

        mod run {
            use super::super::super::*;
            use crate::output_mapping::*;

            #[test]
            fn run_can_lead_to_and_io_error() {
                let res = Command::new("foo", ReturnNothing)
                    .with_mock_result(|_, _, _| Err(io::Error::new(io::ErrorKind::Other, "random")))
                    .run();

                res.unwrap_err();
            }

            #[test]
            fn return_no_error_if_the_command_has_zero_exit_status() {
                let res = Command::new("foo", ReturnNothing)
                    .with_mock_result(move |_, _, _| {
                        Ok(ExecResult {
                            exit_status: 0.into(),
                            ..Default::default()
                        })
                    })
                    .run();

                res.unwrap();
            }
        }

        mod spawn {
            use std::sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            };

            use mock::MockResultFn;

            use crate::{
                mock::{MockResult, MockSpawn},
                output_mapping::*,
            };

            use super::super::super::*;

            #[test]
            fn can_spawn_and_then_await_outputs() {
                let child = Command::new("foo", ReturnStdoutString)
                    .with_mock_result(move |_, _, _| {
                        Ok(ExecResult {
                            exit_status: 0.into(),
                            stdout: Some("hy".to_owned().into()),
                            ..Default::default()
                        })
                    })
                    .spawn()
                    .unwrap();

                let res = child.wait().unwrap();
                assert_eq!(res, "hy");
            }

            #[test]
            fn implements_debug() {
                let dbg_out = format!("{:?}", Command::new("foo", ReturnNothing));
                assert!(dbg_out.starts_with("Command {"));
                assert!(dbg_out.ends_with("}"));
                for field in &[
                    "spawn_options:",
                    "expected_exit_status:",
                    "output_mapping:",
                    "spawn_impl:",
                ] {
                    assert!(dbg_out.contains(field))
                }
            }

            #[test]
            fn spawn_failure_and_wait_failure_are_seperate() {
                Command::new("foo", ReturnNothing)
                    .with_spawn_impl(MockSpawn::new(|_, _, _| {
                        Err(io::Error::new(io::ErrorKind::Other, "failed spawn"))
                    }))
                    .spawn()
                    .unwrap_err();

                let child = Command::new("foo", ReturnNothing)
                    .with_spawn_impl(MockSpawn::new(|_, _, _| {
                        Ok(MockResult::new(Err(io::Error::new(
                            io::ErrorKind::Other,
                            "failed wait",
                        ))))
                    }))
                    .spawn()
                    .unwrap();

                child.wait().unwrap_err();
            }

            #[test]
            fn spawn_already_spawns_wait_only_awaits_completion() {
                let is_running = Arc::new(AtomicBool::new(false));
                let child = Command::new("foo", ReturnNothing)
                    .with_spawn_impl({
                        let is_running = is_running.clone();
                        MockSpawn::new(move |_, _, _| {
                            let is_running = is_running.clone();
                            is_running.store(true, Ordering::SeqCst);
                            Ok(MockResultFn::new(move || {
                                is_running.store(false, Ordering::SeqCst);
                                Ok(ExecResult {
                                    exit_status: 0.into(),
                                    ..Default::default()
                                })
                            }))
                        })
                    })
                    .spawn()
                    .unwrap();

                assert_eq!(is_running.load(Ordering::SeqCst), true);

                let () = child.wait().unwrap();

                assert_eq!(is_running.load(Ordering::SeqCst), false);
            }
        }

        mod OutputMapping {
            use std::collections::HashMap;

            use super::super::super::*;
            use super::super::TestOutputMapping;
            use crate::{env::EnvBuilder, output_mapping::*};
            use proptest::prelude::*;

            #[test]
            fn comp_command_must_only_be_generic_over_the_output() {
                if false {
                    let mut _cmd = Command::new("foo", ReturnNothing);
                    _cmd = Command::new("foo", ReturnNothingAlt);
                }

                //---
                struct ReturnNothingAlt;
                impl OutputMapping for ReturnNothingAlt {
                    type Output = ();
                    type Error = CommandExecutionError;
                    fn needs_captured_stdout(&self) -> bool {
                        false
                    }
                    fn needs_captured_stderr(&self) -> bool {
                        false
                    }
                    fn map_output(
                        self: Box<Self>,
                        _result: ExecResult,
                    ) -> Result<Self::Output, Self::Error> {
                        unimplemented!()
                    }
                }
            }

            #[test]
            fn allow_custom_errors() {
                let _result: MyError = Command::new("foo", ReturnError)
                    .with_mock_result(|_, _, _| {
                        Ok(ExecResult {
                            exit_status: 0.into(),
                            ..Default::default()
                        })
                    })
                    .run()
                    .unwrap_err();

                //------------
                struct ReturnError;
                impl OutputMapping for ReturnError {
                    type Output = ();
                    type Error = MyError;
                    fn needs_captured_stdout(&self) -> bool {
                        false
                    }
                    fn needs_captured_stderr(&self) -> bool {
                        false
                    }
                    fn map_output(
                        self: Box<Self>,
                        _result: ExecResult,
                    ) -> Result<Self::Output, Self::Error> {
                        Err(MyError::BarFoot)
                    }
                }
                #[derive(Debug, Error)]
                enum MyError {
                    #[error("FooBar")]
                    BarFoot,

                    #[error(transparent)]
                    Io(#[from] io::Error),

                    #[error(transparent)]
                    UnexpectedExitStatus(#[from] UnexpectedExitStatus),
                }
            }

            #[test]
            fn returning_stdout_even_if_needs_captured_stdout_does_not_panic() {
                let _ = Command::new("foo", ReturnNothing)
                    .without_expected_exit_status()
                    .with_mock_result(|_, _, _| {
                        Ok(ExecResult {
                            exit_status: 1.into(),
                            stdout: Some(Vec::new()),
                            ..Default::default()
                        })
                    })
                    .run();
            }
            #[test]
            fn returning_stderr_even_if_needs_captured_stderr_does_not_panic() {
                let _ = Command::new("foo", ReturnNothing)
                    .without_expected_exit_status()
                    .with_mock_result(|_, _, _| {
                        Ok(ExecResult {
                            exit_status: 1.into(),
                            stderr: Some(Vec::new()),
                            ..Default::default()
                        })
                    })
                    .run();
            }

            fn assert_eq_env_updates(
                builder: &EnvBuilder,
                map: &HashMap<OsString, EnvUpdate>,
            ) -> Result<(), proptest::test_runner::TestCaseError> {
                let inspector = builder.env_updates_iter();
                assert_eq!(inspector.len(), map.len());

                for (k, v) in inspector {
                    prop_assert_eq!(Some(v), map.get(k), "for key {:?}", k);
                }

                Ok(())
            }

            proptest! {
                #[test]
                fn only_pass_stdout_stderr_to_map_output_if_return_settings_indicate_they_capture_it(
                    capture_stdout in proptest::bool::ANY,
                    capture_stderr in proptest::bool::ANY
                ) {
                    let res = Command::new("foo", TestOutputMapping { capture_stdout, capture_stderr })
                        .with_mock_result(move |_,_,_| {
                            Ok(ExecResult {
                                exit_status: 0.into(),
                                stdout: if capture_stdout { Some(Vec::new()) } else { None },
                                stderr: if capture_stderr { Some(Vec::new()) } else { None }
                            })
                        })
                        .run()
                        .map_err(|e| e.unwrap_prop())?;

                    assert!(res);
                }

                #[test]
                fn command_provides_a_getter_to_check_if_stdout_and_err_will_likely_be_captured(
                    capture_stdout in proptest::bool::ANY,
                    capture_stderr in proptest::bool::ANY
                ) {
                    let cmd = Command::new("foo", TestOutputMapping { capture_stdout, capture_stderr });
                    prop_assert_eq!(cmd.will_capture_stdout(), capture_stdout);
                    prop_assert!(cmd.custom_stdout_setup.is_none());
                    prop_assert_eq!(cmd.will_capture_stderr(), capture_stderr);
                    prop_assert!(cmd.custom_stderr_setup.is_none());
                }


                #[test]
                fn capture_hints_are_available_in_the_callback(
                    capture_stdout in proptest::bool::ANY,
                    capture_stderr in proptest::bool::ANY
                ) {
                    Command::new("foo", TestOutputMapping { capture_stdout, capture_stderr })
                        .with_mock_result(move |_, capture_stdout_hint, capture_stderr_hint| {
                            assert_eq!(capture_stdout_hint, capture_stdout);
                            assert_eq!(capture_stderr_hint, capture_stderr);
                            Ok(ExecResult {
                                exit_status: 0.into(),
                                stdout: if capture_stdout_hint { Some(Vec::new()) } else { None },
                                stderr: if capture_stderr_hint { Some(Vec::new()) } else { None },
                            })
                        })
                        .run()
                        .unwrap();
                }

                #[test]
                fn new_env_variables_can_be_added(
                    cmd in any::<OsString>(),
                    variable in any::<OsString>(),
                    value in any::<OsString>(),
                    map1 in proptest::collection::hash_map(
                        any::<OsString>(),
                        any::<OsString>().prop_map(|s| EnvUpdate::Set(s)),
                        0..4
                    ),
                    map2 in proptest::collection::hash_map(
                        any::<OsString>(),
                        any::<OsString>().prop_map(|s| EnvUpdate::Set(s)),
                        0..4
                    ),
                ) {
                    let cmd = Command::new(cmd, ReturnNothing)
                        .with_env_updates(&map1);

                    assert_eq_env_updates(&cmd.env_builder, &map1)?;

                    let cmd = cmd.with_env_update(&variable, &value);

                    let mut n_map = map1.clone();
                    n_map.insert(variable, EnvUpdate::Set(value));
                    assert_eq_env_updates(&cmd.env_builder, &n_map)?;

                    let cmd = cmd.with_env_updates(&map2);

                    for (key, value) in &map2 {
                        n_map.insert(key.into(), value.into());
                    }
                    assert_eq_env_updates(&cmd.env_builder, &n_map)?;
                }
            }
        }
        mod environment {
            use crate::output_mapping::*;
            use std::collections::HashMap;

            use super::super::super::*;

            #[test]
            fn by_default_no_environment_updates_are_done() {
                let cmd = Command::new("foo", ReturnNothing);
                assert_eq!(cmd.env_builder.env_updates_iter().len(), 0);
            }

            #[test]
            fn create_expected_env_iter_includes_the_current_env_by_default() {
                let process_env = ::std::env::vars_os().into_iter().collect::<HashMap<_, _>>();
                let cmd = Command::new("foo", ReturnNothing);
                let mut created_map = HashMap::new();
                cmd.env_builder.clone().build_on(&mut created_map);
                assert_eq!(process_env, created_map);
            }

            #[test]
            fn by_default_env_is_inherited() {
                let cmd = Command::new("foo", ReturnNothing);
                assert_eq!(cmd.env_builder.inherit_env(), true);
                //FIXME fluky if there is no single ENV variable set
                //But this kinda can't happen as the test environment set's some
                let mut env_map = HashMap::new();
                cmd.env_builder.clone().build_on(&mut env_map);
                assert_ne!(env_map.len(), 0);
            }

            #[test]
            fn inheritance_of_env_variables_can_be_disabled() {
                let cmd = Command::new("foo", ReturnNothing).with_inherit_env(false);
                let mut env_map = HashMap::new();
                cmd.env_builder.clone().build_on(&mut env_map);
                assert_eq!(env_map.len(), 0);
            }
        }

        mod working_directory {
            use super::super::super::*;
            use crate::{output_mapping::*, utils::opt_arbitrary_path_buf};
            use proptest::prelude::*;

            #[test]
            fn by_default_no_explicit_working_directory_is_set() {
                let cmd = Command::new("foo", ReturnNothing);
                assert_eq!(cmd.working_directory_override.as_ref(), None);
            }

            proptest! {
                #[test]
                fn the_working_directory_can_be_changed(
                    cmd in any::<OsString>(),
                    wd_override in opt_arbitrary_path_buf(),
                    wd_override2 in opt_arbitrary_path_buf()
                ) {
                    let cmd = Command::new(cmd, ReturnNothing)
                        .with_working_directory_override(wd_override.as_ref());

                    assert_eq!(cmd.working_directory_override.as_ref(), wd_override.as_ref());

                    let cmd = cmd.with_working_directory_override(wd_override2.as_ref());
                    assert_eq!(cmd.working_directory_override.as_ref(), wd_override2.as_ref());
                }
            }
        }

        mod exit_status_checking {
            use super::super::super::*;
            use crate::output_mapping::*;
            use proptest::prelude::*;

            #[test]
            fn by_default_the_expected_exit_status_is_0() {
                let cmd = Command::new("foo", ReturnNothing);
                assert_eq!(cmd.expected_exit_status.as_ref().unwrap(), &0);
            }

            #[test]
            fn by_default_exit_status_checking_is_enabled() {
                let cmd = Command::new("foo", ReturnNothing);
                assert_eq!(cmd.expected_exit_status.is_some(), true);
            }

            #[test]
            fn setting_check_exit_status_to_false_disables_it() {
                Command::new("foo", ReturnNothing)
                    .without_expected_exit_status()
                    .with_mock_result(|_, _, _| {
                        Ok(ExecResult {
                            exit_status: 1.into(),
                            ..Default::default()
                        })
                    })
                    .run()
                    .unwrap();
            }

            #[test]
            fn you_can_expect_no_exit_status_to_be_returned() {
                let cmd = Command::new("foo", ReturnNothing).with_expected_exit_status(
                    ExitStatus::OsSpecific(OpaqueOsExitStatus::target_specific_default()),
                );

                assert_eq!(
                    &cmd.expected_exit_status,
                    &Some(ExitStatus::OsSpecific(
                        OpaqueOsExitStatus::target_specific_default()
                    ))
                );
            }

            #[test]
            fn setting_the_expected_exit_status_will_enable_checking() {
                let cmd = Command::new("foo", ReturnNothing)
                    .without_expected_exit_status()
                    .with_expected_exit_status(0);

                assert_eq!(cmd.expected_exit_status.is_some(), true);
            }

            proptest! {
                #[test]
                fn return_an_error_if_the_command_has_non_zero_exit_status(
                    cmd in any::<OsString>(),
                    exit_status in prop_oneof!(..0, 1..).prop_map(ExitStatus::from)
                ) {
                    let res = Command::new(cmd, ReturnNothing)
                        .with_mock_result(move |_,_,_| {
                            Ok(ExecResult {
                                exit_status,
                                ..Default::default()
                            })
                        })
                        .run();

                    res.unwrap_err();
                }

                #[test]
                fn replacing_the_expected_exit_status_causes_error_on_different_exit_status(
                    exit_status in -5..6,
                    offset in prop_oneof!(-100..0, 1..101)
                ) {
                    let res = Command::new("foo", ReturnNothing)
                        .with_expected_exit_status(exit_status)
                        .with_mock_result(move |_,_,_| {
                            Ok(ExecResult {
                                exit_status: ExitStatus::from(exit_status + offset),
                                ..Default::default()
                            })
                        })
                        .run();

                    match res {
                        Err(CommandExecutionError::UnexpectedExitStatus(UnexpectedExitStatus {got, expected})) => {
                            assert_eq!(expected, exit_status);
                            assert_eq!(got, exit_status+offset);
                        },
                        _ => panic!("Unexpected Result: {:?}", res)
                    }
                }
            }
        }

        mod exec_replacement_callback {
            use std::sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            };

            use output_mapping::ReturnStdoutAndErr;

            use super::super::super::*;

            #[test]
            fn program_execution_can_be_replaced_with_an_callback() {
                let was_run = Arc::new(AtomicBool::new(false));
                let was_run_ = was_run.clone();
                let cmd = Command::new("some_cmd", ReturnStdoutAndErr).with_mock_result(
                    move |options, capture_stdout, capture_stderr| {
                        assert_eq!(capture_stdout, true);
                        assert_eq!(capture_stderr, true);
                        was_run_.store(true, Ordering::SeqCst);
                        assert_eq!(&options.program, "some_cmd");
                        Ok(ExecResult {
                            exit_status: 0.into(),
                            stdout: Some("result=12".to_owned().into()),
                            stderr: Some(Vec::new()),
                        })
                    },
                );

                let res = cmd.run().unwrap();
                assert_eq!(was_run.load(Ordering::SeqCst), true);
                assert_eq!(&*res.stdout, "result=12".as_bytes());
                assert_eq!(&*res.stderr, "".as_bytes());
            }
        }
    }

    mod Child {
        #![allow(non_snake_case)]
        use output_mapping::ReturnNothing;

        use super::super::*;
        //most parts are already tested by `Command`

        #[test]
        fn impl_debug() {
            let child = Command::new("foo", ReturnNothing)
                .with_mock_result(|_, _, _| {
                    Ok(ExecResult {
                        exit_status: 0.into(),
                        ..Default::default()
                    })
                })
                .spawn()
                .unwrap();

            let dbg_out = format!("{:?}", child);
            assert!(dbg_out.starts_with("Child {"));
            assert!(dbg_out.ends_with("}"));
            for field in &["expected_exit_status:", "output_mapping:", "child:"] {
                assert!(dbg_out.contains(field));
            }
        }
    }
}
