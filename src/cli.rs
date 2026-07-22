use std::cmp::max;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::rc::Rc;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum, ValueHint};
use log::{LevelFilter, error};
use serde::{Deserialize, Serialize};
use toml::Value;
use vivido_config::SerdeReplace;

use crate::terminal::tty::Options as PtyOptions;

use crate::config::UiConfig;
use crate::config::ui_config::Program;
use crate::config::window::{Class, Identity};
use crate::logging::LOG_TARGET_IPC_CONFIG;

/// CLI options for the main Vivido executable.
#[derive(Parser, Default, Debug)]
#[clap(author, about, version = env!("VERSION"))]
pub struct Options {
    /// Print all events to STDOUT.
    #[clap(long)]
    pub print_events: bool,

    /// Generates ref test.
    #[clap(long, conflicts_with("daemon"))]
    pub ref_test: bool,

    /// Specify alternative configuration file [default:
    /// $XDG_CONFIG_HOME/vivido/vivido.toml].
    #[cfg(not(windows))]
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub config_file: Option<PathBuf>,

    /// Specify alternative configuration file [default: %USERPROFILE%\vivido\vivido.toml].
    #[cfg(windows)]
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub config_file: Option<PathBuf>,

    /// Path for IPC socket creation.
    #[cfg(unix)]
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub socket: Option<PathBuf>,

    /// Reduces the level of verbosity (the min level is -qq).
    #[clap(short, conflicts_with("verbose"), action = ArgAction::Count)]
    quiet: u8,

    /// Increases the level of verbosity (the max level is -vvv).
    #[clap(short, conflicts_with("quiet"), action = ArgAction::Count)]
    verbose: u8,

    /// Do not spawn an initial window.
    #[clap(long)]
    pub daemon: bool,

    /// CLI options for config overrides.
    #[clap(skip)]
    pub config_options: ParsedOptions,

    /// Options which can be passed via IPC.
    #[clap(flatten)]
    pub window_options: WindowOptions,

    /// Subcommand passed to the CLI.
    #[clap(subcommand)]
    pub subcommands: Option<Subcommands>,
}

impl Options {
    pub fn new() -> Self {
        let mut options = Self::parse();

        // Parse CLI config overrides.
        options.config_options = options.window_options.config_overrides();

        options
    }

    /// Override configuration file with options from the CLI.
    pub fn override_config(&mut self, config: &mut UiConfig) {
        #[cfg(unix)]
        if self.socket.is_some() {
            config.ipc_socket = Some(true);
        }

        config.debug.print_events |= self.print_events;
        config.debug.log_level = max(config.debug.log_level, self.log_level());
        config.debug.ref_test |= self.ref_test;

        if config.debug.print_events {
            config.debug.log_level = max(config.debug.log_level, LevelFilter::Info);
        }

        // Replace CLI options.
        self.config_options.override_config(config);
    }

    /// Logging filter level.
    pub fn log_level(&self) -> LevelFilter {
        match (self.quiet, self.verbose) {
            // Force at least `Info` level for `--print-events`.
            (_, 0) if self.print_events => LevelFilter::Info,

            // Default.
            (0, 0) => LevelFilter::Warn,

            // Verbose.
            (_, 1) => LevelFilter::Info,
            (_, 2) => LevelFilter::Debug,
            (0, _) => LevelFilter::Trace,

            // Quiet.
            (1, _) => LevelFilter::Error,
            (..) => LevelFilter::Off,
        }
    }
}

/// Parse the class CLI parameter.
fn parse_class(input: &str) -> Result<Class, String> {
    let (general, instance) = match input.split_once(',') {
        // Warn the user if they've passed too many values.
        Some((_, instance)) if instance.contains(',') => {
            return Err(String::from("Too many parameters"));
        },
        Some((general, instance)) => (general, instance),
        None => (input, input),
    };

    Ok(Class::new(general, instance))
}

/// Terminal specific cli options which can be passed to new windows via IPC.
#[derive(Serialize, Deserialize, Args, Default, Debug, Clone, PartialEq, Eq)]
pub struct TerminalOptions {
    /// Start the shell in the specified working directory.
    #[clap(long, value_hint = ValueHint::FilePath)]
    pub working_directory: Option<PathBuf>,

    /// Remain open after child process exit.
    #[clap(long)]
    pub hold: bool,

    /// Command and args to execute (must be last argument).
    #[clap(short = 'e', long, allow_hyphen_values = true, num_args = 1..)]
    command: Vec<String>,
}

impl TerminalOptions {
    /// Shell override passed through the CLI.
    pub fn command(&self) -> Option<Program> {
        let (program, args) = self.command.split_first()?;
        Some(Program::WithArgs { program: program.clone(), args: args.to_vec() })
    }

    /// Override the [`PtyOptions`]'s fields with the [`TerminalOptions`].
    pub fn override_pty_config(&self, pty_config: &mut PtyOptions) {
        if let Some(working_directory) = &self.working_directory {
            if working_directory.is_dir() {
                pty_config.working_directory = Some(working_directory.to_owned());
            } else {
                error!("Invalid working directory: {working_directory:?}");
            }
        }

        if let Some(command) = self.command() {
            pty_config.shell = Some(command.into());
        }

        pty_config.drain_on_exit |= self.hold;
    }
}

impl From<TerminalOptions> for PtyOptions {
    fn from(mut options: TerminalOptions) -> Self {
        PtyOptions {
            working_directory: options.working_directory.take(),
            shell: options.command().map(Into::into),
            drain_on_exit: options.hold,
            env: HashMap::new(),
            #[cfg(target_os = "windows")]
            escape_args: false,
        }
    }
}

/// Window specific cli options which can be passed to new windows via IPC.
#[derive(Serialize, Deserialize, Args, Default, Debug, Clone, PartialEq, Eq)]
pub struct WindowIdentity {
    /// Defines the window title [default: Vivido].
    #[clap(short = 'T', short_alias('t'), long)]
    pub title: Option<String>,

    /// Defines the Wayland app_id [default: Vivido].
    #[clap(long, value_name = "general> | <general>,<instance", value_parser = parse_class)]
    pub class: Option<Class>,
}

impl WindowIdentity {
    /// Override the [`WindowIdentity`]'s fields with the [`WindowOptions`].
    pub fn override_identity_config(&self, identity: &mut Identity) {
        if let Some(title) = &self.title {
            identity.title.clone_from(title);
        }
        if let Some(class) = &self.class {
            identity.class.clone_from(class);
        }
    }
}

/// Available CLI subcommands.
#[derive(Subcommand, Debug)]
pub enum Subcommands {
    #[cfg(unix)]
    Msg(MessageOptions),
    Migrate(MigrateOptions),
}

/// Send a message to the Vivido socket.
#[cfg(unix)]
#[derive(Args, Debug)]
pub struct MessageOptions {
    /// IPC socket connection path override.
    #[clap(short, long, value_hint = ValueHint::FilePath)]
    pub socket: Option<PathBuf>,

    /// Message which should be sent.
    #[clap(subcommand)]
    pub message: SocketMessage,
}

/// Available socket messages.
#[cfg(unix)]
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum SocketMessage {
    /// Create a new window in the same Vivido process.
    CreateWindow(WindowOptions),

    /// Update the Vivido configuration.
    Config(IpcConfig),

    /// Read runtime Vivido configuration.
    GetConfig(IpcGetConfig),

    /// Type literal text into a terminal.
    Typing(IpcTyping),

    /// Read terminal text.
    GetText(IpcGetText),

    /// Capture the last displayed terminal frame.
    Screenshot(IpcScreenshot),

    /// Print supported automation methods, events, and limits.
    Capabilities,

    /// Send one mode-aware key to a terminal.
    Key(IpcKey),

    /// Paste literal text into a terminal.
    Paste(IpcPaste),

    /// Send a mouse action to a terminal or Vivido UI.
    Mouse(IpcMouse),

    /// Resize a terminal window.
    Resize(IpcResize),

    /// Request real operating-system focus for a window.
    Focus(IpcTarget),

    /// Send an explicit signal to the foreground process group.
    Signal(IpcSignal),

    /// List all windows in deterministic creation order.
    ListWindows,

    /// Inspect one terminal window.
    Inspect(IpcTarget),

    /// Read a structured terminal grid snapshot or delta.
    GetGrid(IpcGetGrid),

    /// Wait for terminal state or output.
    Wait(IpcWait),

    /// Read retained sanitized PTY output.
    Transcript(IpcTranscript),

    /// Stream automation events until interrupted.
    Subscribe(IpcSubscribe),
}

/// Migrate the configuration file.
#[derive(Args, Clone, Debug)]
pub struct MigrateOptions {
    /// Path to the configuration file.
    #[clap(short, long, value_hint = ValueHint::FilePath)]
    pub config_file: Option<PathBuf>,

    /// Only output TOML config to STDOUT.
    #[clap(short, long)]
    pub dry_run: bool,

    /// Do not recurse over imports.
    #[clap(short = 'i', long)]
    pub skip_imports: bool,

    /// Do not move renamed fields to their new location.
    #[clap(long)]
    pub skip_renames: bool,

    #[clap(short, long)]
    /// Do not output to STDOUT.
    pub silent: bool,
}

/// Subset of options that we pass to 'create-window' IPC subcommand.
#[derive(Serialize, Deserialize, Args, Default, Clone, Debug, PartialEq, Eq)]
pub struct WindowOptions {
    /// Stable IPC ID assigned to this window.
    ///
    /// When omitted, Vivido uses the platform window ID.
    #[cfg(unix)]
    #[clap(short = 'w', long = "window-id", value_name = "WINDOW_ID")]
    pub ipc_window_id: Option<u64>,

    /// Terminal options which can be passed via IPC.
    #[clap(flatten)]
    pub terminal_options: TerminalOptions,

    #[clap(flatten)]
    /// Window options which could be passed via IPC.
    pub window_identity: WindowIdentity,

    #[clap(skip)]
    #[cfg(target_os = "macos")]
    /// The window tabbing identifier to use when building a window.
    pub window_tabbing_id: Option<String>,

    #[clap(skip)]
    #[cfg(not(any(target_os = "macos", windows)))]
    /// `ActivationToken` that we pass to winit.
    pub activation_token: Option<String>,

    /// Override configuration file options [example: 'cursor.style="Beam"'].
    #[clap(short = 'o', long, num_args = 1..)]
    option: Vec<String>,
}

impl WindowOptions {
    /// Get the parsed set of CLI config overrides.
    pub fn config_overrides(&self) -> ParsedOptions {
        ParsedOptions::from_options(&self.option)
    }
}

/// Parameters to the `config` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcConfig {
    /// Configuration file options [example: 'cursor.style="Beam"'].
    #[clap(required = true, value_name = "CONFIG_OPTIONS")]
    pub options: Vec<String>,

    /// Window ID for the new config.
    ///
    /// Use `-1` to apply this change to all windows.
    #[clap(short, long, allow_hyphen_values = true, env = "VIVIDO_WINDOW_ID")]
    pub window_id: Option<i128>,

    /// Clear all runtime configuration changes.
    #[clap(short, long, conflicts_with = "options")]
    pub reset: bool,
}

/// Parameters to the `get-config` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcGetConfig {
    /// Window ID for the config request.
    ///
    /// Use `-1` to get the global config.
    #[clap(short, long, allow_hyphen_values = true, env = "VIVIDO_WINDOW_ID")]
    pub window_id: Option<i128>,
}

/// Parameters to the `typing` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct IpcTyping {
    /// Literal UTF-8 text to write to the terminal PTY.
    #[clap(required = true, value_name = "TEXT", allow_hyphen_values = true)]
    pub text: String,

    /// Window ID for terminal input.
    ///
    /// The focused window is used when no ID is specified.
    #[clap(short, long, env = "VIVIDO_WINDOW_ID")]
    pub window_id: Option<u64>,
}

#[cfg(unix)]
impl std::fmt::Debug for IpcTyping {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IpcTyping")
            .field("text", &format_args!("<{} bytes>", self.text.len()))
            .field("window_id", &self.window_id)
            .finish()
    }
}

/// Parameters to the `get-text` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcGetText {
    /// Number of latest physical terminal rows to return.
    ///
    /// The current visible viewport is returned when this is omitted.
    #[clap(long, value_parser = clap::value_parser!(u16).range(1..=1000))]
    pub rows: Option<u16>,

    /// Window ID for terminal text.
    ///
    /// The focused window is used when no ID is specified.
    #[clap(short, long, env = "VIVIDO_WINDOW_ID")]
    pub window_id: Option<u64>,
}

/// Parameters to the `screenshot` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcScreenshot {
    /// Window ID for the screenshot.
    ///
    /// The focused window is used when no ID is specified.
    #[clap(short, long, env = "VIVIDO_WINDOW_ID")]
    pub window_id: Option<u64>,
}

/// Common target selection for IPC commands.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcTarget {
    /// Window ID. The focused window is used when this is omitted.
    #[clap(short, long, env = "VIVIDO_WINDOW_ID")]
    pub window_id: Option<u64>,
}

/// Route for injected input.
#[cfg(unix)]
#[derive(ValueEnum, Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcInputRoute {
    /// Bypass Vivido bindings and encode input for the terminal application.
    #[default]
    Application,
    /// Process input through Vivido's normal UI input pipeline.
    Ui,
}

/// Parameters to the `key` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcKey {
    /// Unicode scalar or named key (Enter, Escape, ArrowUp, F1, and so on).
    #[clap(value_name = "KEY")]
    pub key: String,

    /// Comma-separated Ctrl, Alt, Shift, and Super modifiers.
    #[clap(long, value_delimiter = ',')]
    pub mods: Vec<String>,

    /// Number of key presses to send.
    #[clap(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=1000))]
    pub repeat: u16,

    /// Input routing mode.
    #[clap(long, value_enum, default_value_t)]
    pub route: IpcInputRoute,

    #[clap(flatten)]
    pub target: IpcTarget,
}

/// Parameters to the `paste` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct IpcPaste {
    /// Literal UTF-8 text to paste.
    #[clap(required = true, value_name = "TEXT", allow_hyphen_values = true)]
    pub text: String,

    /// Input routing mode.
    #[clap(long, value_enum, default_value_t)]
    pub route: IpcInputRoute,

    #[clap(flatten)]
    pub target: IpcTarget,
}

#[cfg(unix)]
impl std::fmt::Debug for IpcPaste {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IpcPaste")
            .field("text", &format_args!("<{} bytes>", self.text.len()))
            .field("route", &self.route)
            .field("target", &self.target)
            .finish()
    }
}

/// Mouse coordinate and modifier arguments.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct IpcMousePosition {
    /// Zero-based terminal cell column.
    #[clap(long, requires = "cell_row", conflicts_with_all = ["x", "y"])]
    pub cell_column: Option<u32>,

    /// Zero-based terminal cell row.
    #[clap(long, requires = "cell_column", conflicts_with_all = ["x", "y"])]
    pub cell_row: Option<u32>,

    /// Physical-pixel X coordinate in the client area.
    #[clap(long, requires = "y", conflicts_with_all = ["cell_column", "cell_row"])]
    pub x: Option<f64>,

    /// Physical-pixel Y coordinate in the client area.
    #[clap(long, requires = "x", conflicts_with_all = ["cell_column", "cell_row"])]
    pub y: Option<f64>,

    /// Comma-separated Ctrl, Alt, Shift, and Super modifiers.
    #[clap(long, value_delimiter = ',')]
    pub mods: Vec<String>,

    /// Input routing mode.
    #[clap(long, value_enum, default_value_t)]
    pub route: IpcInputRoute,

    #[clap(flatten)]
    pub target: IpcTarget,
}

/// Mouse button accepted by IPC.
#[cfg(unix)]
#[derive(ValueEnum, Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcMouseButton {
    #[default]
    Left,
    Middle,
    Right,
}

/// Mouse arguments requiring a button.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IpcMouseButtonAction {
    /// Mouse button.
    #[clap(long, value_enum, default_value_t)]
    pub button: IpcMouseButton,

    #[clap(flatten)]
    pub position: IpcMousePosition,
}

/// Mouse scrolling arguments.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IpcMouseScroll {
    /// Vertical scroll amount; positive values scroll up.
    #[clap(long, default_value_t = 0.0)]
    pub vertical: f64,

    /// Horizontal scroll amount; positive values scroll left.
    #[clap(long, default_value_t = 0.0)]
    pub horizontal: f64,

    #[clap(flatten)]
    pub position: IpcMousePosition,
}

/// Available mouse actions.
#[cfg(unix)]
#[derive(Subcommand, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IpcMouseAction {
    Move(IpcMousePosition),
    Click(IpcMouseButtonAction),
    DoubleClick(IpcMouseButtonAction),
    Down(IpcMouseButtonAction),
    Up(IpcMouseButtonAction),
    Drag(IpcMouseButtonAction),
    Scroll(IpcMouseScroll),
}

/// Parameters to the `mouse` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IpcMouse {
    #[clap(subcommand)]
    pub action: IpcMouseAction,
}

/// Parameters to the `resize` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcResize {
    /// Exact terminal grid column count.
    #[clap(long, requires = "rows", conflicts_with_all = ["width", "height"], value_parser = clap::value_parser!(u16).range(2..))]
    pub columns: Option<u16>,

    /// Exact terminal grid row count.
    #[clap(long, requires = "columns", conflicts_with_all = ["width", "height"], value_parser = clap::value_parser!(u16).range(1..))]
    pub rows: Option<u16>,

    /// Exact physical client width in pixels.
    #[clap(long, requires = "height", conflicts_with_all = ["columns", "rows"], value_parser = clap::value_parser!(u32).range(1..))]
    pub width: Option<u32>,

    /// Exact physical client height in pixels.
    #[clap(long, requires = "width", conflicts_with_all = ["columns", "rows"], value_parser = clap::value_parser!(u32).range(1..))]
    pub height: Option<u32>,

    #[clap(flatten)]
    pub target: IpcTarget,
}

/// Explicit Unix signals accepted by IPC.
#[cfg(unix)]
#[derive(ValueEnum, Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum IpcSignalName {
    Int,
    Term,
    Hup,
    Quit,
    Tstp,
    Cont,
    Winch,
    Kill,
    Stop,
}

/// Parameters to the `signal` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcSignal {
    /// Signal name without a SIG prefix.
    #[clap(value_enum, ignore_case = true)]
    pub signal: IpcSignalName,

    #[clap(flatten)]
    pub target: IpcTarget,
}

/// Parameters to the `get-grid` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcGetGrid {
    /// First signed physical grid line in retained scrollback/live-screen coordinates.
    #[clap(
        long,
        allow_hyphen_values = true,
        requires = "row_count",
        conflicts_with = "since_screen"
    )]
    pub start_line: Option<i32>,

    /// Number of physical rows to return.
    #[clap(long, requires = "start_line", conflicts_with = "since_screen", value_parser = clap::value_parser!(u16).range(1..=1000))]
    pub row_count: Option<u16>,

    /// Return current viewport row replacements changed after this screen sequence.
    #[clap(long)]
    pub since_screen: Option<u64>,

    #[clap(flatten)]
    pub target: IpcTarget,
}

fn parse_ipc_duration(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        (value, 1)
    };
    let number = number.parse::<u64>().map_err(|_| format!("invalid duration {value:?}"))?;
    let milliseconds =
        number.checked_mul(multiplier).ok_or_else(|| format!("duration {value:?} is too large"))?;
    if (1..=86_400_000).contains(&milliseconds) {
        Ok(milliseconds)
    } else {
        Err(String::from("duration must be between 1ms and 24h"))
    }
}

/// Common timeout for wait commands, represented as milliseconds on the wire.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcWaitCommon {
    /// Maximum wait (for example 500ms, 30s, 2m, or 1h).
    #[clap(long, default_value = "30s", value_parser = parse_ipc_duration)]
    pub timeout: u64,

    #[clap(flatten)]
    pub target: IpcTarget,
}

/// Text wait parameters.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcWaitText {
    pub text: String,
    #[clap(long)]
    pub regex: bool,
    #[clap(long)]
    pub after_screen: Option<u64>,
    #[clap(flatten)]
    pub common: IpcWaitCommon,
}

/// Output wait parameters.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcWaitOutput {
    pub pattern: String,
    #[clap(long, conflicts_with = "base64")]
    pub regex: bool,
    #[clap(long, conflicts_with = "regex")]
    pub base64: bool,
    #[clap(long)]
    pub after_offset: Option<u64>,
    #[clap(flatten)]
    pub common: IpcWaitCommon,
}

/// Screen/frame sequence wait parameters.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcWaitSequence {
    #[clap(long)]
    pub after_screen: Option<u64>,
    #[clap(flatten)]
    pub common: IpcWaitCommon,
}

/// Screen stability wait parameters.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcWaitStable {
    /// Required period without semantic screen changes.
    #[clap(long, default_value = "250ms", value_parser = parse_ipc_duration)]
    pub quiet: u64,
    #[clap(long)]
    pub after_screen: Option<u64>,
    #[clap(flatten)]
    pub common: IpcWaitCommon,
}

/// Frame wait parameters.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcWaitFrame {
    #[clap(long)]
    pub after_frame: Option<u64>,
    #[clap(flatten)]
    pub common: IpcWaitCommon,
}

/// Available wait conditions.
#[cfg(unix)]
#[derive(Subcommand, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcWaitCondition {
    Text(IpcWaitText),
    Output(IpcWaitOutput),
    ScreenChange(IpcWaitSequence),
    ScreenStable(IpcWaitStable),
    Frame(IpcWaitFrame),
    Exit(IpcWaitCommon),
}

/// Parameters to the `wait` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcWait {
    #[clap(subcommand)]
    pub condition: IpcWaitCondition,
}

/// Parameters to the `transcript` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IpcTranscript {
    /// First retained byte offset. Omit to request the newest bytes.
    #[clap(long)]
    pub after_offset: Option<u64>,

    /// Maximum returned byte count.
    #[clap(long, default_value_t = 65_536, value_parser = clap::value_parser!(u32).range(1..=1_048_576))]
    pub max_bytes: u32,

    /// Write exact decoded bytes instead of JSON metadata.
    #[clap(long)]
    pub raw: bool,

    #[clap(flatten)]
    pub target: IpcTarget,
}

/// Parameters to the `subscribe` IPC subcommand.
#[cfg(unix)]
#[derive(Args, Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct IpcSubscribe {
    /// Window ID. The focused window is used when omitted.
    #[clap(short, long, env = "VIVIDO_WINDOW_ID", conflicts_with = "all")]
    pub window_id: Option<u64>,

    /// Subscribe to every window and process lifecycle event.
    #[clap(long, conflicts_with = "window_id")]
    pub all: bool,

    /// Comma-separated event kinds. Omit for all kinds.
    #[clap(long, value_delimiter = ',')]
    pub events: Vec<String>,

    /// Replay matching events newer than this global event sequence.
    #[clap(long)]
    pub since_event: Option<u64>,
}

/// Parsed CLI config overrides.
#[derive(Debug, Default)]
pub struct ParsedOptions {
    config_options: Vec<(String, Value)>,
}

impl ParsedOptions {
    /// Parse CLI config overrides.
    pub fn from_options(options: &[String]) -> Self {
        let mut config_options = Vec::new();

        for option in options {
            let parsed = match toml::from_str(option) {
                Ok(parsed) => parsed,
                Err(err) => {
                    eprintln!("Ignoring invalid CLI option '{option}': {err}");
                    continue;
                },
            };
            config_options.push((option.clone(), parsed));
        }

        Self { config_options }
    }

    /// Apply CLI config overrides, removing broken ones.
    pub fn override_config(&mut self, config: &mut UiConfig) {
        let mut i = 0;
        while i < self.config_options.len() {
            let (option, parsed) = &self.config_options[i];
            match config.replace(parsed.clone()) {
                Err(err) => {
                    error!(
                        target: LOG_TARGET_IPC_CONFIG,
                        "Unable to override option '{option}': {err}"
                    );
                    self.config_options.swap_remove(i);
                },
                Ok(_) => i += 1,
            }
        }
    }

    /// Apply CLI config overrides to a CoW config.
    pub fn override_config_rc(&mut self, config: Rc<UiConfig>) -> Rc<UiConfig> {
        // Skip clone without write requirement.
        if self.config_options.is_empty() {
            return config;
        }

        // Override cloned config.
        let mut config = (*config).clone();
        self.override_config(&mut config);

        Rc::new(config)
    }
}

impl Deref for ParsedOptions {
    type Target = Vec<(String, Value)>;

    fn deref(&self) -> &Self::Target {
        &self.config_options
    }
}

impl DerefMut for ParsedOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config_options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::fs::File;
    #[cfg(unix)]
    use std::io::{Read, Write};

    #[cfg(unix)]
    use clap::CommandFactory;
    #[cfg(unix)]
    use clap_complete::Shell;
    use toml::Table;

    #[test]
    fn dynamic_title_ignoring_options_by_default() {
        let mut config = UiConfig::default();
        let old_dynamic_title = config.window.dynamic_title;

        Options::default().override_config(&mut config);

        assert_eq!(old_dynamic_title, config.window.dynamic_title);
    }

    #[test]
    fn dynamic_title_not_overridden_by_config() {
        let mut config = UiConfig::default();

        config.window.identity.title = "foo".to_owned();
        Options::default().override_config(&mut config);

        assert!(config.window.dynamic_title);
    }

    #[test]
    fn valid_option_as_value() {
        // Test with a single field.
        let value: Value = toml::from_str("field=true").unwrap();

        let mut table = Table::new();
        table.insert(String::from("field"), Value::Boolean(true));

        assert_eq!(value, Value::Table(table));

        // Test with nested fields
        let value: Value = toml::from_str("parent.field=true").unwrap();

        let mut parent_table = Table::new();
        parent_table.insert(String::from("field"), Value::Boolean(true));
        let mut table = Table::new();
        table.insert(String::from("parent"), Value::Table(parent_table));

        assert_eq!(value, Value::Table(table));
    }

    #[test]
    fn invalid_option_as_value() {
        let value = toml::from_str::<Value>("}");
        assert!(value.is_err());
    }

    #[test]
    fn float_option_as_value() {
        let value: Value = toml::from_str("float=3.4").unwrap();

        let mut expected = Table::new();
        expected.insert(String::from("float"), Value::Float(3.4));

        assert_eq!(value, Value::Table(expected));
    }

    #[test]
    fn parse_instance_class() {
        let class = parse_class("one").unwrap();
        assert_eq!(class.general, "one");
        assert_eq!(class.instance, "one");
    }

    #[test]
    fn parse_general_class() {
        let class = parse_class("one,two").unwrap();
        assert_eq!(class.general, "one");
        assert_eq!(class.instance, "two");
    }

    #[test]
    fn parse_invalid_class() {
        let class = parse_class("one,two,three");
        assert!(class.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn parse_typing_message() {
        let options =
            Options::try_parse_from(["vivido", "msg", "typing", "--window-id", "42", "echo hello"])
                .unwrap();

        let Some(Subcommands::Msg(message)) = options.subcommands else {
            panic!("expected msg subcommand");
        };
        assert_eq!(
            message.message,
            SocketMessage::Typing(IpcTyping {
                text: String::from("echo hello"),
                window_id: Some(42),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn parse_assigned_window_ids() {
        let options = Options::try_parse_from(["vivido", "--window-id", "1234"]).unwrap();
        assert_eq!(options.window_options.ipc_window_id, Some(1234));

        let options =
            Options::try_parse_from(["vivido", "msg", "create-window", "--window-id", "5678"])
                .unwrap();
        let Some(Subcommands::Msg(message)) = options.subcommands else {
            panic!("expected msg subcommand");
        };
        let SocketMessage::CreateWindow(window_options) = message.message else {
            panic!("expected create-window message");
        };
        assert_eq!(window_options.ipc_window_id, Some(5678));
    }

    #[cfg(unix)]
    #[test]
    fn typing_debug_redacts_input() {
        let typing = IpcTyping { text: String::from("secret"), window_id: Some(42) };
        let debug = format!("{typing:?}");

        assert!(!debug.contains("secret"));
        assert!(debug.contains("6 bytes"));
    }

    #[cfg(unix)]
    #[test]
    fn parse_capture_messages() {
        let options = Options::try_parse_from([
            "vivido",
            "msg",
            "get-text",
            "--rows",
            "1000",
            "--window-id",
            "42",
        ])
        .unwrap();
        let Some(Subcommands::Msg(message)) = options.subcommands else {
            panic!("expected msg subcommand");
        };
        assert_eq!(
            message.message,
            SocketMessage::GetText(IpcGetText { rows: Some(1000), window_id: Some(42) })
        );

        let options =
            Options::try_parse_from(["vivido", "msg", "screenshot", "--window-id", "42"]).unwrap();
        let Some(Subcommands::Msg(message)) = options.subcommands else {
            panic!("expected msg subcommand");
        };
        assert_eq!(
            message.message,
            SocketMessage::Screenshot(IpcScreenshot { window_id: Some(42) })
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_rows_are_bounded() {
        assert!(Options::try_parse_from(["vivido", "msg", "get-text", "--rows", "0"]).is_err());
        assert!(Options::try_parse_from(["vivido", "msg", "get-text", "--rows", "1001"]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn parse_agent_control_and_wait_commands() {
        let options = Options::try_parse_from([
            "vivido",
            "msg",
            "key",
            "Enter",
            "--mods",
            "Ctrl,Shift",
            "--repeat",
            "3",
            "--route",
            "application",
            "--window-id",
            "42",
        ])
        .unwrap();
        let Some(Subcommands::Msg(message)) = options.subcommands else {
            panic!("expected msg subcommand");
        };
        let SocketMessage::Key(key) = message.message else {
            panic!("expected key message");
        };
        assert_eq!(key.key, "Enter");
        assert_eq!(key.mods, ["Ctrl", "Shift"]);
        assert_eq!(key.repeat, 3);
        assert_eq!(key.target.window_id, Some(42));

        let options = Options::try_parse_from([
            "vivido",
            "msg",
            "wait",
            "screen-stable",
            "--quiet",
            "500ms",
            "--timeout",
            "2m",
        ])
        .unwrap();
        let Some(Subcommands::Msg(message)) = options.subcommands else {
            panic!("expected msg subcommand");
        };
        let SocketMessage::Wait(wait) = message.message else {
            panic!("expected wait message");
        };
        let IpcWaitCondition::ScreenStable(wait) = wait.condition else {
            panic!("expected screen-stable wait");
        };
        assert_eq!(wait.quiet, 500);
        assert_eq!(wait.common.timeout, 120_000);
    }

    #[cfg(unix)]
    #[test]
    fn agent_cli_limits_are_rejected() {
        assert!(
            Options::try_parse_from(["vivido", "msg", "key", "a", "--repeat", "1001"]).is_err()
        );
        assert!(
            Options::try_parse_from(["vivido", "msg", "wait", "frame", "--timeout", "0ms"])
                .is_err()
        );
        assert!(
            Options::try_parse_from(["vivido", "msg", "wait", "frame", "--timeout", "25h"])
                .is_err()
        );
    }

    // The checked-in completion files describe the Unix-only socket and `msg`
    // surface, so generating them from the reduced Windows CLI is not a valid
    // snapshot comparison.
    #[cfg(unix)]
    #[test]
    fn completions() {
        let mut clap = Options::command();

        for (shell, file) in
            &[(Shell::Bash, "vivido.bash"), (Shell::Fish, "vivido.fish"), (Shell::Zsh, "_vivido")]
        {
            let mut generated = Vec::new();
            clap_complete::generate(*shell, &mut clap, "vivido", &mut generated);
            let generated = String::from_utf8_lossy(&generated);

            let path =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("extra/completions").join(file);
            if std::env::var_os("VIVIDO_GENERATE_COMPLETIONS").is_some() {
                File::create(&path).unwrap().write_all(generated.as_bytes()).unwrap();
                continue;
            }

            let mut completion = String::new();
            let mut file = File::open(path).unwrap();
            file.read_to_string(&mut completion).unwrap();

            assert_eq!(generated, completion);
        }
    }
}
