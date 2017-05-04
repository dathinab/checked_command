


use std::fmt::Debug;
use std::process::{Command, ExitStatus, Child};
use std::process::{Output as StdOutput};
use std::io::Error as IoError;
use std::io::Result as IoResult;

#[cfg(use_std_output)]
pub type Output = StdOutput;

#[cfg(not(use_std_output))]
#[derive(Debug, Clone, Eq, PartialEq)]
/// custom output type which, diferently to `std::process::Output` is not
/// closely coupled with `ExitStatus` (it does not have a `status` field)
pub struct Output {
    /// the collected output of stdout as bytes
    pub stdout: Vec<u8>,
    /// the collected output of stderr as bytes
    pub stderr: Vec<u8>
}

#[cfg(not(use_std_output))]
impl From<StdOutput> for Output {
    fn from(out: StdOutput) -> Output {
        Output {
            stdout: out.stdout,
            stderr: out.stderr
        }
    }
}

/// Extension to `std::process::Command` adding versions of the output/status
/// functions which also fail/error with a non-success exit status
pub trait CommandExt {
    /// Behaves like `std::process::Command::output` but also checks the
    /// exit status for success. The `Output` produced in case of a
    /// command returning a failing exit status is included into the
    /// error.
    ///
    /// # Error
    ///
    /// if the exit status is not successful or a `io::Error` was returned
    /// from `Command::output`
    ///
    fn checked_output(&mut self) -> Result<Output, StatusErrorWithOutput>;

    /// Behaves like `std::process::Command::status` but also checks the
    /// exit status for success, returning a error if it did not succeed
    ///
    /// # Error
    ///
    /// if the exit status is not successful or a `io::Error` was returned
    /// from `Command::output`
    ///
    fn checked_status(&mut self) -> Result<(), StatusError>;
}

/// Extension to `std::process::Child` adding versions of the wait_with_output/wait
/// functions which also fail/error with a non-success exit status
pub trait ChildExt {

    /// Behaves like `std::process::Child::wait_with_output` but also
    /// checks the exit status for success. The `Output` produced in
    /// case of a command returning a failing exit status is included
    /// into the error.
    ///
    /// # Error
    ///
    /// if the exit status is not successful or a `io::Error` was returned
    /// from `Child::wait_with_output`
    ///
    fn checked_wait_with_output(self) -> Result<Output, StatusErrorWithOutput>;

    /// Behaves like `std::process::Child::wait` but also checks the
    /// exit status for success.
    ///
    /// # Error
    ///
    /// if the exit status is not successful or a `io::Error` was returned
    /// from `Child::checked_wait`
    ///
    fn checked_wait(&mut self) -> Result<(), StatusError>;

    /// Behaves like `std::process::child::try_wait` but also checks the
    /// exit status for success.
    ///
    /// This means `Ok(true)` is returned if the process exited and
    /// did with a successful exit status. If it did not exit jet `Ok(false)` is
    /// returned. If it exited but had a non successful exit status `Err(StatusError)`
    /// with the `StatusError::Failure` variant is returned.
    ///
    /// # Error
    ///
    /// if the exit status can be retrived but is not successful or
    /// a `io::Error` was returned from `Child::checked_try_wait`
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::process::Command;
    /// use checked_command::{ChildExt, StatusError};
    ///
    ///
    /// let mut child = Command::new("ls").spawn().unwrap();
    ///
    /// match child.checked_try_wait() {
    ///     Ok(true) => println!("exited with successful status (== 0)"),
    ///     Ok(false) => {
    ///         println!("status not ready yet, let's really wait");
    ///         let res = child.checked_wait();
    ///         println!("result: {:?}", res);
    ///     }
    ///     Err(StatusError::Io(e)) => println!("error when attempting to wait for `ls` {}", e),
    ///     Err(StatusError::Failure(exit_status)) => {
    ///         println!("ls failed with exit code {:?}", exit_status.code())
    ///     }
    /// }
    /// ```
    #[cfg(feature="process_try_wait")]
    fn checked_try_wait(&mut self) -> Result<bool, StatusError>;
}


impl CommandExt for Command {
    fn checked_output(&mut self) -> Result<Output, StatusErrorWithOutput> {
        convert_result(self.output())
    }
    fn checked_status(&mut self) -> Result<(), StatusError> {
        convert_result(self.status())
    }
}

impl ChildExt for Child {
    fn checked_wait_with_output(self) -> Result<Output, StatusErrorWithOutput> {
        convert_result(self.wait_with_output())
    }
    fn checked_wait(&mut self) -> Result<(), StatusError> {
        convert_result(self.wait())
    }

    #[cfg(feature="process_try_wait")]
    fn checked_try_wait(&mut self) -> Result<bool, StatusError> {
        convert_result(self.try_wait())
    }
}

macro_rules! def_error {
    ($(#[$attr:meta])* def $name:ident, $ex:ident => $($part:tt)*) => {
        quick_error! {
            $(#[$attr])*
            #[derive(Debug)]
            pub enum $name {
                /// a `io::Error` occurred when handling the action
                Io(err: IoError) {
                    from()
                    description(err.description())
                    cause(err)
                }
                /// Process exited with a non-zero exit status
                Failure($($part)*) {
                    description("command failed with nonzero exit code")
                    display("command failed with exit status {}", $ex.code()
                        .map(|code|code.to_string())
                        .unwrap_or_else(||"<None> possible terminated by signal".into()))
                }
            }
        }
    }
}


def_error!{
    /// error returned from the checked `status`/`wait` method variations
    /// as they will never contain a output this error has no `Output`
    /// filed
    def StatusError, ex => ex: ExitStatus }

def_error!{
    /// error returned from the checked `output`/`wait_with_output` method variations
    /// as ther is always a Output in the `Failure` case it has a `Output` filed
    def StatusErrorWithOutput, ex => ex: ExitStatus, output: Output }

def_error!{
    /// error combining `StatusError` and `StatusErrorWithOutput`. It can optionally
    /// have a `Output`, but the field might be `None`. It is not returned
    /// by any command execution function, but both `StatusError` and `StatusErrorWithOutput`
    /// can be converted into it using `From::from`/`Into::into`.
    def Error, ex => ex: ExitStatus, output: Option<Output> }


impl From<StatusError> for Error {

    fn from(err: StatusError) -> Error {
        match err {
            StatusError::Io(io_err) => Error::Io(io_err),
            StatusError::Failure(ex) => Error::Failure(ex, None)
        }
    }
}

impl From<StatusErrorWithOutput> for Error {

    fn from(err: StatusErrorWithOutput) -> Error {
        match err {
            StatusErrorWithOutput::Io(io_err) => Error::Io(io_err),
            StatusErrorWithOutput::Failure(ex, output) => Error::Failure(ex, Some(output))
        }
    }
}




/// internal trait to zero-cost abstract
/// over handling `IoResult<Output>`` or
/// `IoResult<ExitStatus>``. It's a bit
/// over enginered but through zero-cost
/// abstraction (Type Extensions+Inlining, ExitStatus is Copy)
/// it should not introduce any overhad
/// at runtime
trait OutputOrExitStatus: Sized {
    type Error: From<IoError>;
    type Out;
    fn use_ok_result(&self) -> bool;
    fn create_error(self) -> Self::Error;
    fn convert(self) -> Self::Out;
}

#[cfg(feature="process_try_wait")]
impl OutputOrExitStatus for Option<ExitStatus> {
    type Error = StatusError;
    type Out = bool;

    #[inline]
    fn use_ok_result(&self) -> bool { self.is_none() || self.unwrap().success() }

    #[inline]
    fn create_error(self) -> StatusError {
        //we can call unwrap as a None option won't lead to this branch
        StatusError::Failure(self.unwrap())
    }

    #[inline]
    fn convert(self) -> bool {
        self.is_some()
    }
}

impl OutputOrExitStatus for ExitStatus {
    type Error = StatusError;
    type Out = ();

    #[inline]
    fn use_ok_result(&self) -> bool {
        self.success()
    }

    #[inline]
    fn create_error(self) -> StatusError {
        StatusError::Failure(self)
    }

    #[inline]
    fn convert(self) -> () {
        ()
    }
}

impl OutputOrExitStatus for StdOutput {
    type Error = StatusErrorWithOutput;
    type Out = Output;

    #[inline]
    fn use_ok_result(&self) -> bool { self.status.success() }

    #[inline]
    fn create_error(self) -> StatusErrorWithOutput {
        // because of the abstraction we got a Option but we can relay on
        // it to always be `Some(Output)` as long as this function is
        // not exported
        StatusErrorWithOutput::Failure(self.status, self.into())
    }

    #[inline]
    fn convert(self) -> Output {
        self.into()
    }
}


/// works with both Output and `ExitStatus`
/// **without** introducing any clones or similar
/// which would not have been needed for
/// specialized methods
fn convert_result<T>(result: IoResult<T>) -> Result<T::Out, T::Error>
    where T: OutputOrExitStatus + Debug
{
    match result {
        Ok(think) => {
            if think.use_ok_result() {
                Ok(think.convert())
            } else {
                Err(think.create_error())
            }
        },
        Err(io_error) => Err(io_error.into())
    }
}

#[cfg(test)]
mod tests {

    // this crate on itself is doesn't care about unix/windows,
    // it's only that I need some "example" commands to create exit
    // stati I then can use for testing. Therefore I "limited" the
    // test to unix where I _should_ be able to relay on `/usr/bin/ls`
    // in the test environment
    #[cfg(unix)]
    mod using_unix_cmds {

        use std::io;
        use std::process::{ExitStatus, Command,  Stdio};
        use std::process::{Output as StdOutput};
        use super::super::{convert_result, Error, StatusError, Output, StatusErrorWithOutput};

        use tutils::assert_debugstr_eq;

        lazy_static! {
            static ref OK_STATUS: ExitStatus = {
                Command::new("/usr/bin/ls")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status().expect("your unix does not have ls?!")
            };
            static ref ERR_STATUS: ExitStatus = {
                Command::new("/usr/bin/ls")
                    .arg("--nononono")
                    // makes sure it works like expected even if there is
                    // a folder/file named "--nononono"
                    .arg("--")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status().expect("your unix does not have ls?!")
            };
        }

        #[test]
        fn stati_are_as_they_should() {
            assert!(OK_STATUS.success());
            assert!(!ERR_STATUS.success());
            assert_eq!(2, ERR_STATUS.code().unwrap());
        }

        #[test]
        fn conv_result_status_ok() {
            let res = convert_result(Ok(*OK_STATUS));
            assert_debugstr_eq(Ok(()), res);
        }

        #[test]
        fn conv_result_status_fail() {
            let res = convert_result(Ok(*ERR_STATUS));
            assert_debugstr_eq(Err(StatusError::Failure(*ERR_STATUS)), res);
        }

        #[test]
        fn conv_result_status_io_error() {
            let ioerr = io::Error::new(io::ErrorKind::Other, "bla");
            let ioerr2 = io::Error::new(io::ErrorKind::Other, "bla");
            let res: Result<(), StatusError> = convert_result::<ExitStatus>(Err(ioerr));
            assert_debugstr_eq(
                Err(StatusError::Io(ioerr2)),
                res
            )
        }

        #[test]
        fn conv_result_output_io_error() {
            let ioerr = io::Error::new(io::ErrorKind::Other, "bla");
            let ioerr2 = io::Error::new(io::ErrorKind::Other, "bla");
            let res: Result<Output, StatusErrorWithOutput> = convert_result::<StdOutput>(Err(ioerr));
            assert_debugstr_eq(
                Err(StatusErrorWithOutput::Io(ioerr2)),
                res
            )
        }

        fn create_output(ex: ExitStatus) -> StdOutput {
            StdOutput {
                status: ex,
                stderr: vec![1,2,3],
                stdout: vec![1,2,3]
            }
        }

        #[test]
        fn conv_result_output_ok() {
            let out = create_output(*OK_STATUS);
            let out2 = out.clone();
            assert_debugstr_eq(Ok(out2.into()), convert_result(Ok(out)));
        }

        #[test]
        fn conv_result_output_fail() {
            let out = create_output(*ERR_STATUS);
            let out2 = out.clone();
            assert_debugstr_eq(
                Err(StatusErrorWithOutput::Failure(*ERR_STATUS, out2.into())),
                convert_result(Ok(out))
            )
        }

        #[cfg(feature="process_try_wait")]
        #[test]
        fn conv_result_not_ready() {
            match convert_result(Ok(None)) {
                Ok(false) => {},
                e => panic!("expected `Ok(false)` got `{:?}`", e)
            }
        }

        #[cfg(feature="process_try_wait")]
        #[test]
        fn conv_result_ready_ok() {
            match convert_result(Ok(Some(*OK_STATUS))) {
                Ok(true) => {},
                e => panic!("expected `Ok(true)` got `{:?}`", e)
            }
        }

        #[cfg(feature="process_try_wait")]
        #[test]
        fn conv_result_ready_failure() {
            let res = convert_result(Ok(Some(*ERR_STATUS)));
            assert_debugstr_eq(Err(StatusError::Failure(*ERR_STATUS)), res);
        }

        #[test]
        fn error_from_status_error_io() {
            let gen_io_err = ||io::Error::new(io::ErrorKind::Other, "ups");
            let serr = StatusError::Io(gen_io_err());
            let err: Error = serr.into();
            let io_err = match err {
                Error::Io(io_err) => io_err,
                Error::Failure(_, _) => panic!("unexpected From conversion")
            };

            assert_debugstr_eq(
                gen_io_err(),
                io_err
            )
        }

        #[test]
        fn error_from_status_error_failure() {
            let serr = StatusError::Failure(*ERR_STATUS);
            let err: Error = serr.into();
            match err {
                Error::Failure(ex, None) => assert_eq!(*ERR_STATUS, ex),
                _ => panic!("unexpected From conversion")
            }
        }

        #[test]
        fn error_from_status_error_wo_io() {
            let gen_io_err = ||io::Error::new(io::ErrorKind::Other, "ups");
            let serr = StatusErrorWithOutput::Io(gen_io_err());
            let err: Error = serr.into();
            let io_err = match err {
                Error::Io(io_err) => io_err,
                Error::Failure(_, _) => panic!("unexpected From conversion")
            };

            assert_debugstr_eq(
                gen_io_err(),
                io_err
            )
        }

        #[test]
        fn error_from_status_error_wo_failure() {
            let serr = StatusErrorWithOutput::Failure(
                *ERR_STATUS,
                create_output(*ERR_STATUS).into()
            );
            let err: Error = serr.into();
            match err {
                Error::Failure(ex, Some(output)) => {
                    assert_eq!(*ERR_STATUS, ex);
                    assert_eq!(vec![1,2,3], output.stdout);
                    assert_eq!(vec![1,2,3], output.stderr);
                },
                _ => panic!("unexpected From conversion")
            }
        }
    }

}