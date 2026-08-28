use std::{
    collections::BTreeSet,
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

const GO_IMPORT_PATH_ENVIRONMENT: &str = "TOOLEXEC_IMPORTPATH";
const MAX_ARGUMENTS: usize = 4_096;
const MAX_ARGUMENT_BYTES: usize = 16 * 1_024;
const MAX_GO_FILES: usize = 512;
const MAX_GO_SOURCE_BYTES: u64 = 4 * 1_024 * 1_024;
const MAX_TOOL_VERSION_BYTES: usize = 4_096;
const TIME_IMPORT_PATH: &str = "time";
const TIME_SOURCE_NAME: &str = "time.go";
const SYSCALL_IMPORT_PATH: &str = "syscall";
const TOOL_IDENTITY: &str = "reproit-go-instrumentation-v2";

const ORIGINAL_NOW_DIRECTIVE: &str = "//go:linkname Now\n";
const ORIGINAL_NOW_DECLARATION: &str = "func Now() Time {";
const INSTRUMENTED_NOW_SOURCE: &str = concat!(
    r#"

//go:linkname reproitInstrumentedNow reproit.dev/sdk-go/reproit.instrumentedTimeNow
func reproitInstrumentedNow() Time

"#,
    "//go:linkname reproitRegisterClockInstrumentation ",
    "reproit.dev/sdk-go/reproit.registerAutomaticClockInstrumentationV1\n",
    r#"
func reproitRegisterClockInstrumentation(func() Time)

//go:linkname Now
func Now() Time {
	return reproitInstrumentedNow()
}

func init() {
	reproitRegisterClockInstrumentation(reproitOriginalNow)
}
"#,
);

const ORIGINAL_SETENV_DECLARATION: &str = "func Setenv(key, value string) error {";
const ORIGINAL_UNSETENV_DECLARATION: &str = "func Unsetenv(key string) error {";
const ORIGINAL_CLEARENV_DECLARATION: &str = "func Clearenv() {";
const INSTRUMENTED_ENVIRONMENT_SOURCE: &str = concat!(
    r#"

//go:linkname reproitInstrumentedSetenv reproit.dev/sdk-go/reproit.instrumentedSetenv
func reproitInstrumentedSetenv(string, string) error

//go:linkname reproitInstrumentedUnsetenv reproit.dev/sdk-go/reproit.instrumentedUnsetenv
func reproitInstrumentedUnsetenv(string) error

//go:linkname reproitInstrumentedClearenv reproit.dev/sdk-go/reproit.instrumentedClearenv
func reproitInstrumentedClearenv()

"#,
    "//go:linkname reproitRegisterEnvironmentInstrumentation ",
    "reproit.dev/sdk-go/reproit.registerAutomaticEnvironmentInstrumentationV1\n",
    r#"
func reproitRegisterEnvironmentInstrumentation(
	func(string, string) error,
	func(string) error,
	func(),
)

func Setenv(key, value string) error {
	return reproitInstrumentedSetenv(key, value)
}

func Unsetenv(key string) error {
	return reproitInstrumentedUnsetenv(key)
}

func Clearenv() {
	reproitInstrumentedClearenv()
}

func init() {
	reproitRegisterEnvironmentInstrumentation(
		reproitOriginalSetenv,
		reproitOriginalUnsetenv,
		reproitOriginalClearenv,
	)
}
"#,
);

#[derive(Clone, Copy)]
enum InstrumentationTarget {
    Clock,
    Environment,
}

pub(crate) fn is_invocation(arguments: &[OsString]) -> bool {
    let known_tool = arguments
        .get(1)
        .and_then(|argument| Path::new(argument).file_name())
        .is_some_and(known_go_tool);
    known_tool
        && (env::var_os(GO_IMPORT_PATH_ENVIRONMENT).is_some()
            || arguments.get(2) == Some(&OsString::from("-V=full")))
}

pub(crate) fn run(arguments: &[OsString]) -> ExitCode {
    match run_inner(arguments) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "reproit: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_inner(arguments: &[OsString]) -> Result<ExitCode, &'static str> {
    validate_arguments(arguments)?;
    let tool = PathBuf::from(&arguments[1]);
    let tool_arguments = &arguments[2..];
    if tool_arguments == [OsStr::new("-V=full")] {
        return version(&tool);
    }
    let import_path = env::var(GO_IMPORT_PATH_ENVIRONMENT)
        .map_err(|_| "The Go instrumentation import path is invalid.")?;
    if tool.file_name() != Some(OsStr::new("compile")) {
        return execute(&tool, tool_arguments);
    }
    let target = match import_path.as_str() {
        TIME_IMPORT_PATH => InstrumentationTarget::Clock,
        SYSCALL_IMPORT_PATH => InstrumentationTarget::Environment,
        _ => return execute(&tool, tool_arguments),
    };
    compile_instrumented_package(&tool, tool_arguments, target)
}

fn validate_arguments(arguments: &[OsString]) -> Result<(), &'static str> {
    if arguments.len() < 2 || arguments.len() > MAX_ARGUMENTS {
        return Err("The Go instrumentation argument count is invalid.");
    }
    if arguments.iter().any(|argument| {
        argument.to_string_lossy().len() > MAX_ARGUMENT_BYTES
            || argument.to_string_lossy().chars().any(char::is_control)
    }) {
        return Err("A Go instrumentation argument is invalid.");
    }
    Ok(())
}

fn version(tool: &Path) -> Result<ExitCode, &'static str> {
    let output = Command::new(tool)
        .arg("-V=full")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "The Go tool version check failed.")?;
    if !output.status.success()
        || output.stdout.is_empty()
        || output.stdout.len() > MAX_TOOL_VERSION_BYTES
        || output.stderr.len() > MAX_TOOL_VERSION_BYTES
    {
        return Err("The Go tool version is invalid.");
    }
    io::stderr()
        .lock()
        .write_all(&output.stderr)
        .map_err(|_| "The Go tool version error output failed.")?;
    let version = std::str::from_utf8(&output.stdout)
        .map_err(|_| "The Go tool version is invalid.")?
        .trim_end();
    if version.lines().count() != 1 {
        return Err("The Go tool version is invalid.");
    }
    writeln!(io::stdout().lock(), "{version} {TOOL_IDENTITY}")
        .map_err(|_| "The Go tool version output failed.")?;
    Ok(ExitCode::SUCCESS)
}

fn compile_instrumented_package(
    tool: &Path,
    arguments: &[OsString],
    instrumentation: InstrumentationTarget,
) -> Result<ExitCode, &'static str> {
    let temporary = tempfile::Builder::new()
        .prefix("reproit-go-instrumentation-")
        .tempdir()
        .map_err(|_| "The Go instrumentation workspace could not be created.")?;
    let mut rewritten = arguments.to_vec();
    let mut names = BTreeSet::new();
    let mut source_count = 0_usize;
    let mut transformed = false;
    for argument in &mut rewritten {
        let source = Path::new(argument);
        if source.extension() != Some(OsStr::new("go")) {
            continue;
        }
        source_count = source_count.saturating_add(1);
        if source_count > MAX_GO_FILES {
            return Err("The Go source file count exceeds the instrumentation limit.");
        }
        let name = source
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or("A Go source file name is invalid.")?;
        if !names.insert(name.to_owned()) {
            return Err("The Go source file names are ambiguous.");
        }
        let metadata =
            fs::symlink_metadata(source).map_err(|_| "A Go source file cannot be read.")?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_GO_SOURCE_BYTES {
            return Err("A Go source file is invalid.");
        }
        let bytes = fs::read(source).map_err(|_| "A Go source file cannot be read.")?;
        let output = match instrumentation {
            InstrumentationTarget::Clock if name == TIME_SOURCE_NAME => {
                transformed = true;
                transform_time_source(&bytes)?
            }
            InstrumentationTarget::Environment if name.starts_with("env_") => {
                match transform_environment_source(&bytes) {
                    Ok(value) => {
                        transformed = true;
                        value
                    }
                    Err(_) => bytes,
                }
            }
            _ => bytes,
        };
        let target = temporary.path().join(name);
        fs::write(&target, output).map_err(|_| "A Go source file cannot be staged.")?;
        *argument = target.into_os_string();
    }
    if source_count == 0 || !transformed {
        return Err("The Go standard package cannot be instrumented.");
    }
    execute(tool, &rewritten)
}

fn transform_environment_source(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let source = std::str::from_utf8(bytes).map_err(|_| "The Go environment source is invalid.")?;
    if source.matches(ORIGINAL_SETENV_DECLARATION).count() != 1
        || source.matches(ORIGINAL_UNSETENV_DECLARATION).count() != 1
        || source.matches(ORIGINAL_CLEARENV_DECLARATION).count() != 1
        || source.contains("reproitInstrumentedSetenv")
    {
        return Err("The Go environment source does not match the supported structure.");
    }
    let source = source.replace(
        ORIGINAL_SETENV_DECLARATION,
        "func reproitOriginalSetenv(key, value string) error {",
    );
    let source = source.replace(
        ORIGINAL_UNSETENV_DECLARATION,
        "func reproitOriginalUnsetenv(key string) error {",
    );
    let mut source = source.replace(
        ORIGINAL_CLEARENV_DECLARATION,
        "func reproitOriginalClearenv() {",
    );
    if !source.contains("\"unsafe\"") {
        if source.matches("import (\n").count() != 1 {
            return Err("The Go environment source import block is invalid.");
        }
        source = source.replace("import (\n", "import (\n\t_ \"unsafe\"\n");
    }
    source.push_str(INSTRUMENTED_ENVIRONMENT_SOURCE);
    Ok(source.into_bytes())
}

fn transform_time_source(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let source = std::str::from_utf8(bytes).map_err(|_| "The Go time source is invalid.")?;
    if source.matches(ORIGINAL_NOW_DIRECTIVE).count() != 1
        || source.matches(ORIGINAL_NOW_DECLARATION).count() != 1
        || source.contains("reproitInstrumentedNow")
    {
        return Err("The Go time source does not match the supported structure.");
    }
    let source = source.replace(
        ORIGINAL_NOW_DIRECTIVE,
        "// Repro It retains this implementation for live clock reads.\n",
    );
    let mut source = source.replace(ORIGINAL_NOW_DECLARATION, "func reproitOriginalNow() Time {");
    source.push_str(INSTRUMENTED_NOW_SOURCE);
    Ok(source.into_bytes())
}

fn execute(tool: &Path, arguments: &[OsString]) -> Result<ExitCode, &'static str> {
    let status = Command::new(tool)
        .args(arguments)
        .status()
        .map_err(|_| "The Go tool could not start.")?;
    Ok(
        match status.code().and_then(|code| u8::try_from(code).ok()) {
            Some(code) => ExitCode::from(code),
            _ => ExitCode::from(1),
        },
    )
}

fn known_go_tool(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            "asm"
                | "cgo"
                | "compile"
                | "cover"
                | "link"
                | "preprofile"
                | "vet"
                | "asm.exe"
                | "cgo.exe"
                | "compile.exe"
                | "cover.exe"
                | "link.exe"
                | "preprofile.exe"
                | "vet.exe"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_transform_replaces_one_clock_entry_and_registers_the_original() {
        let source = concat!(
            "package time\n",
            "import _ \"unsafe\"\n",
            "//go:linkname Now\n",
            "// Now returns the current local time.\n",
            "func Now() Time { return Time{} }\n",
        );
        let transformed =
            String::from_utf8(transform_time_source(source.as_bytes()).unwrap()).unwrap();
        assert!(transformed.contains("func reproitOriginalNow() Time"));
        assert!(transformed.contains("func Now() Time"));
        assert!(transformed.contains("reproitRegisterClockInstrumentation("));
        assert_eq!(transformed.matches("func Now() Time").count(), 1);
    }

    #[test]
    fn time_transform_rejects_missing_duplicate_and_changed_entries() {
        for source in [
            "package time\n",
            concat!(
                "//go:linkname Now\nfunc Now() Time {}\n",
                "//go:linkname Now\nfunc Now() Time {}\n",
            ),
            "//go:linkname Now\nfunc Now(value int) Time {}\n",
        ] {
            assert!(transform_time_source(source.as_bytes()).is_err());
        }
    }

    #[test]
    fn environment_transform_wraps_each_mutation_entry() {
        let source = concat!(
            "package syscall\n",
            "import (\n\t\"sync\"\n)\n",
            "func Setenv(key, value string) error { return nil }\n",
            "func Unsetenv(key string) error { return nil }\n",
            "func Clearenv() {}\n",
        );
        let transformed =
            String::from_utf8(transform_environment_source(source.as_bytes()).unwrap()).unwrap();
        assert!(transformed.contains("func reproitOriginalSetenv("));
        assert!(transformed.contains("func reproitOriginalUnsetenv("));
        assert!(transformed.contains("func reproitOriginalClearenv("));
        assert!(transformed.contains("func Setenv(key, value string) error"));
        assert!(transformed.contains("_ \"unsafe\""));
    }

    #[test]
    fn environment_transform_rejects_partial_and_duplicate_entries() {
        for source in [
            "package syscall\nfunc Setenv(key, value string) error { return nil }\n",
            concat!(
                "func Setenv(key, value string) error { return nil }\n",
                "func Setenv(key, value string) error { return nil }\n",
                "func Unsetenv(key string) error { return nil }\n",
                "func Clearenv() {}\n",
            ),
        ] {
            assert!(transform_environment_source(source.as_bytes()).is_err());
        }
    }
}
