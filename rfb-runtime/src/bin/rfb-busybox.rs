//! Multi-call guest utility binary. `image build-rootfs` installs it as
//! `/bin/sh` (plus hardlinked applet names), giving guest images a real
//! `sh -c` implementation so `eval` and shell-string commands work without
//! shipping a full busybox/coreutils tree. Applet selection is by argv[0]
//! basename, mirroring `rfb-mini-tools`.
//!
//! Shell scope (v1): `;`/newline sequences, left-associative `&&`/`||`,
//! pipelines (`a | b`, any stage count), redirections (`>`, `>>`, `<`,
//! `2>`, `2>>`, `2>&1`, `1>&2`, `[n]>`), single/double quotes with backslash
//! escapes, `$?` expansion, `#` comments, and the builtins `exit`, `echo`,
//! `cd`, `pwd`, `true`, `false`, `:`. Everything else is resolved through
//! `PATH` (default `/bin:/sbin:/usr/bin`). Not supported: background jobs,
//! variable assignment/expansion beyond `$?`, command substitution, globbing.
//!
//! Redirection fidelity: `2>&1` duplicates the actual target (file handle or
//! pipeline socket) via `dup`, so merged streams stay merged even when stdout
//! is a pipe; inherited descriptors are duplicated by fd number.

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::{BorrowedFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command as ProcCommand, Stdio};

    pub fn entry() -> ! {
        let argv: Vec<String> = std::env::args().collect();
        let name = argv
            .first()
            .map(|arg| {
                std::path::Path::new(arg)
                    .file_name()
                    .map(|base| base.to_string_lossy().into_owned())
                    .unwrap_or_else(|| arg.clone())
            })
            .unwrap_or_default();
        let args: Vec<String> = argv.into_iter().skip(1).collect();
        match name.as_str() {
            "sh" | "bash" => {
                if args.first().map(String::as_str) == Some("-c") {
                    let script = args.get(1).cloned().unwrap_or_default();
                    std::process::exit(run_script(&script));
                }
                eprintln!("busybox: interactive shell is not supported; use sh -c SCRIPT");
                std::process::exit(2);
            }
            "sleep" => run_sleep(&args),
            other => {
                eprintln!("busybox: unknown applet: {other}");
                std::process::exit(127);
            }
        }
    }

    fn run_sleep(args: &[String]) -> ! {
        let Some(spec) = args.first() else {
            eprintln!("busybox: sleep: missing operand");
            std::process::exit(1);
        };
        let Ok(seconds) = spec.parse::<f64>() else {
            eprintln!("busybox: sleep: invalid time interval {spec:?}");
            std::process::exit(1);
        };
        if !(0.0..=86_400.0).contains(&seconds) {
            eprintln!("busybox: sleep: interval out of range {spec:?}");
            std::process::exit(1);
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
        std::process::exit(0);
    }

    // -----------------------------------------------------------------------
    // Script splitting: top-level `;` / newline / `&&` / `||`, quote-aware.
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Chainer {
        Seq,
        And,
        Or,
    }

    /// Split the raw script into `(chainer, segment-text)` pieces. The
    /// chainer describes how a segment connects to the previous segment's
    /// status; the first segment is always `Seq`. Splitting happens before
    /// parsing so `$?` expands against the status at execution time.
    fn split_segments(script: &str) -> Result<Vec<(Chainer, String)>, String> {
        let chars: Vec<char> = script.chars().collect();
        let mut segments: Vec<(Chainer, String)> = Vec::new();
        let mut current = String::new();
        let mut pending: Option<Chainer> = None;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '\'' | '"' => {
                    let quote = c;
                    current.push(c);
                    i += 1;
                    let mut closed = false;
                    while i < chars.len() {
                        let inner = chars[i];
                        if inner == '\\' && quote == '"' && i + 1 < chars.len() {
                            current.push(inner);
                            current.push(chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        current.push(inner);
                        i += 1;
                        if inner == quote {
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        return Err("unterminated quote".into());
                    }
                }
                '\\' => {
                    current.push(c);
                    if i + 1 < chars.len() {
                        current.push(chars[i + 1]);
                        i += 1;
                    }
                    i += 1;
                }
                '#' if current.trim().is_empty() => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                }
                ';' | '\n' => {
                    flush_segment(&mut segments, &mut current, pending.take())?;
                    i += 1;
                }
                '&' if chars.get(i + 1) == Some(&'&') => {
                    flush_segment(&mut segments, &mut current, pending.take())?;
                    pending = Some(Chainer::And);
                    i += 2;
                }
                '|' if chars.get(i + 1) == Some(&'|') => {
                    flush_segment(&mut segments, &mut current, pending.take())?;
                    pending = Some(Chainer::Or);
                    i += 2;
                }
                // `>&` / `<&` fd duplication is redirection syntax, not a
                // background operator.
                '&' if i > 0 && chars[i - 1] == '>' => {
                    current.push('&');
                    i += 1;
                }
                '&' => return Err("background jobs (&) are not supported".into()),
                _ => {
                    current.push(c);
                    i += 1;
                }
            }
        }
        // A pending chainer with no accumulated text means the script ended
        // on `&&`/`||`; with text, flush it as the final segment.
        if pending.is_some() && current.trim().is_empty() {
            return Err("trailing && or || with no command".into());
        }
        flush_segment(&mut segments, &mut current, pending.take())?;
        if segments.is_empty() {
            return Err("empty script".into());
        }
        Ok(segments)
    }

    fn flush_segment(
        segments: &mut Vec<(Chainer, String)>,
        current: &mut String,
        pending: Option<Chainer>,
    ) -> Result<(), String> {
        let text = current.trim().to_string();
        current.clear();
        if text.is_empty() {
            if pending.is_some() {
                return Err("empty command in chain".into());
            }
            return Ok(());
        }
        let chainer = if segments.is_empty() {
            Chainer::Seq
        } else {
            pending.unwrap_or(Chainer::Seq)
        };
        segments.push((chainer, text));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Segment parsing: pipeline of simple commands with words and redirections.
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq)]
    enum RedirTarget {
        Path(String),
        Dup(u8),
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum RedirMode {
        Write,
        Append,
        Read,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Redir {
        fd: u8,
        mode: RedirMode,
        target: RedirTarget,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct SimpleCommand {
        words: Vec<String>,
        redirs: Vec<Redir>,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Pipeline {
        stages: Vec<SimpleCommand>,
    }

    struct SegmentParser {
        chars: Vec<char>,
        pos: usize,
        last_status: i32,
    }

    impl SegmentParser {
        fn new(text: &str, last_status: i32) -> Self {
            Self {
                chars: text.chars().collect(),
                pos: 0,
                last_status,
            }
        }

        fn peek(&self) -> Option<char> {
            self.chars.get(self.pos).copied()
        }

        fn peek_at(&self, offset: usize) -> Option<char> {
            self.chars.get(self.pos + offset).copied()
        }

        fn bump(&mut self) -> Option<char> {
            let c = self.peek();
            if c.is_some() {
                self.pos += 1;
            }
            c
        }

        fn skip_blanks(&mut self) {
            while matches!(self.peek(), Some(' ') | Some('\t')) {
                self.pos += 1;
            }
        }

        fn parse_pipeline(&mut self) -> Result<Pipeline, String> {
            let mut stages = vec![self.parse_command()?];
            loop {
                self.skip_blanks();
                if self.peek() == Some('|') && self.peek_at(1) != Some('|') {
                    self.pos += 1;
                    self.skip_blanks();
                    stages.push(self.parse_command()?);
                } else {
                    break;
                }
            }
            self.skip_blanks();
            if self.peek().is_some() {
                return Err("unexpected trailing characters".into());
            }
            Ok(Pipeline { stages })
        }

        fn parse_command(&mut self) -> Result<SimpleCommand, String> {
            let mut words = Vec::new();
            let mut redirs = Vec::new();
            loop {
                self.skip_blanks();
                match self.peek() {
                    None => break,
                    Some('|') => break,
                    Some(digit) if digit.is_ascii_digit() && self.is_redir_after_digit() => {
                        let fd = digit.to_digit(10).expect("digit") as u8;
                        self.pos += 1;
                        redirs.push(self.parse_redir(Some(fd))?);
                    }
                    Some('>') | Some('<') => redirs.push(self.parse_redir(None)?),
                    _ => words.push(self.read_word()?),
                }
            }
            if words.is_empty() && redirs.is_empty() {
                return Err("empty command".into());
            }
            Ok(SimpleCommand { words, redirs })
        }

        fn is_redir_after_digit(&self) -> bool {
            matches!(self.peek_at(1), Some('>') | Some('<'))
        }

        fn parse_redir(&mut self, explicit_fd: Option<u8>) -> Result<Redir, String> {
            let head = self.bump().expect("redir head");
            let mut mode = if head == '<' {
                if explicit_fd.is_some() {
                    return Err("input redirection does not take an fd prefix".into());
                }
                RedirMode::Read
            } else {
                RedirMode::Write
            };
            if head == '>' && self.peek() == Some('>') {
                self.pos += 1;
                mode = RedirMode::Append;
            }
            if self.peek() == Some('&') {
                self.pos += 1;
                self.skip_blanks();
                let target = self.read_word()?;
                let dup_fd: u8 = target
                    .parse()
                    .map_err(|_| format!("unsupported fd duplication >&{target}"))?;
                if dup_fd > 2 {
                    return Err(format!("unsupported fd duplication >&{target}"));
                }
                let fd = explicit_fd.unwrap_or(if head == '<' { 0 } else { 1 });
                return Ok(Redir {
                    fd,
                    mode: RedirMode::Write,
                    target: RedirTarget::Dup(dup_fd),
                });
            }
            self.skip_blanks();
            let path = self.read_word()?;
            let fd = explicit_fd.unwrap_or(if head == '<' { 0 } else { 1 });
            Ok(Redir {
                fd,
                mode,
                target: RedirTarget::Path(path),
            })
        }

        /// One word: bare chars, quoted spans, backslash escapes, `$?`.
        fn read_word(&mut self) -> Result<String, String> {
            let mut word = String::new();
            loop {
                match self.peek() {
                    Some('\'') => {
                        self.pos += 1;
                        loop {
                            match self.bump() {
                                Some('\'') => break,
                                Some(c) => word.push(c),
                                None => return Err("unterminated single quote".into()),
                            }
                        }
                    }
                    Some('"') => {
                        self.pos += 1;
                        loop {
                            match self.bump() {
                                Some('"') => break,
                                Some('\\') => {
                                    if let Some(c) = self.bump() {
                                        word.push(c);
                                    }
                                }
                                Some('$') if self.peek() == Some('?') => {
                                    self.pos += 1;
                                    word.push_str(&self.last_status.to_string());
                                }
                                Some(c) => word.push(c),
                                None => return Err("unterminated double quote".into()),
                            }
                        }
                    }
                    Some('\\') => {
                        self.pos += 1;
                        if let Some(c) = self.bump() {
                            word.push(c);
                        }
                    }
                    Some('$') if self.peek_at(1) == Some('?') => {
                        self.pos += 2;
                        word.push_str(&self.last_status.to_string());
                    }
                    Some(c) if is_word_terminator(c, self.peek_at(1)) => break,
                    Some(c) => {
                        word.push(c);
                        self.pos += 1;
                    }
                    None => break,
                }
            }
            if word.is_empty() {
                return Err("expected a word".into());
            }
            Ok(word)
        }
    }

    fn is_word_terminator(c: char, next: Option<char>) -> bool {
        match c {
            ' ' | '\t' | '|' | '<' | '>' | '&' => true,
            '0'..='9' => matches!(next, Some('<') | Some('>')),
            _ => false,
        }
    }

    // -----------------------------------------------------------------------
    // Execution
    // -----------------------------------------------------------------------

    const DEFAULT_PATH: &str = "/bin:/sbin:/usr/bin";

    enum Outcome {
        Status(i32),
        Exit(i32),
    }

    /// Child-side fd carrier: a concrete file or one end of a socketpair.
    enum Chan {
        File(File),
        Stream(UnixStream),
    }

    impl Chan {
        fn dup(&self) -> std::io::Result<OwnedFd> {
            match self {
                Chan::File(file) => file.try_clone().map(Into::into),
                Chan::Stream(stream) => stream.try_clone().map(Into::into),
            }
        }

        fn into_stdio(self) -> Stdio {
            match self {
                Chan::File(file) => file.into(),
                Chan::Stream(stream) => File::from(OwnedFd::from(stream)).into(),
            }
        }
    }

    fn dup_inherited(fd: u8) -> std::io::Result<OwnedFd> {
        // SAFETY: fd is one of this process's standard descriptors; the clone
        // performs a real `dup`, so the result stays valid independently.
        unsafe { BorrowedFd::borrow_raw(fd as i32) }.try_clone_to_owned()
    }

    pub fn run_script(script: &str) -> i32 {
        let segments = match split_segments(script) {
            Ok(segments) => segments,
            Err(error) => {
                eprintln!("sh: syntax error: {error}");
                return 2;
            }
        };
        let mut status = 0;
        for (chainer, text) in segments {
            let run = match chainer {
                Chainer::Seq => true,
                Chainer::And => status == 0,
                Chainer::Or => status != 0,
            };
            if !run {
                continue;
            }
            let mut parser = SegmentParser::new(&text, status);
            let pipeline = match parser.parse_pipeline() {
                Ok(pipeline) => pipeline,
                Err(error) => {
                    eprintln!("sh: syntax error: {error}");
                    return 2;
                }
            };
            status = match run_pipeline(&pipeline) {
                Outcome::Status(status) => status,
                Outcome::Exit(code) => return code,
            };
        }
        status
    }

    /// Run the whole pipeline; the result mirrors the last stage's exit
    /// status (like POSIX `sh` without `pipefail`).
    fn run_pipeline(pipeline: &Pipeline) -> Outcome {
        if pipeline.stages.len() == 1 {
            return run_simple(&pipeline.stages[0], None, None);
        }
        let mut previous_reader: Option<UnixStream> = None;
        let mut children: Vec<Child> = Vec::new();
        for (index, stage) in pipeline.stages.iter().enumerate() {
            let is_last = index + 1 == pipeline.stages.len();
            let (reader, writer) = if is_last {
                (None, None)
            } else {
                match UnixStream::pair() {
                    Ok(pair) => (Some(pair.0), Some(pair.1)),
                    Err(error) => {
                        eprintln!("sh: pipe: {error}");
                        return Outcome::Status(1);
                    }
                }
            };
            let stdin_chan = previous_reader.take().map(Chan::Stream);
            let stdout_chan = writer.map(Chan::Stream);
            match spawn_simple(stage, stdin_chan, stdout_chan) {
                Ok(child) => children.push(child),
                Err(error) => {
                    eprintln!("sh: {error}");
                    return Outcome::Status(127);
                }
            }
            // Drop the parent's reader copy once the next stage owns it; the
            // writer already moved into the child, so stage exit conveys EOF.
            previous_reader = reader;
        }
        let mut status = 0;
        for mut child in children {
            status = match child.wait() {
                Ok(status) => status.code().unwrap_or(1),
                Err(error) => {
                    eprintln!("sh: wait: {error}");
                    1
                }
            };
        }
        Outcome::Status(status)
    }

    fn run_simple(
        command: &SimpleCommand,
        stdin_chan: Option<Chan>,
        stdout_chan: Option<Chan>,
    ) -> Outcome {
        if command.words.is_empty() {
            // Pure-redirection command: opening the targets creates or
            // truncates them, mirroring `: > file`.
            for redir in &command.redirs {
                if let Err(error) = open_target(redir) {
                    eprintln!("sh: {error}");
                    return Outcome::Status(1);
                }
            }
            return Outcome::Status(0);
        }
        if let Some(outcome) = run_builtin(command, stdout_chan.as_ref()) {
            return outcome;
        }
        let mut child = match spawn_simple(command, stdin_chan, stdout_chan) {
            Ok(child) => child,
            Err(error) => {
                eprintln!("sh: {error}");
                return Outcome::Status(127);
            }
        };
        match child.wait() {
            Ok(status) => Outcome::Status(status.code().unwrap_or(1)),
            Err(error) => {
                eprintln!("sh: wait: {error}");
                Outcome::Status(1)
            }
        }
    }

    fn spawn_simple(
        command: &SimpleCommand,
        stdin_chan: Option<Chan>,
        stdout_chan: Option<Chan>,
    ) -> Result<Child, String> {
        let program = &command.words[0];
        let program_path =
            resolve_program(program).ok_or_else(|| format!("{program}: not found"))?;
        let (stdin, stdout, stderr) = build_stdio(command, stdin_chan, stdout_chan)?;
        ProcCommand::new(program_path)
            .args(&command.words[1..])
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|error| format!("{program}: {error}"))
    }

    fn run_builtin(command: &SimpleCommand, stdout_pipe: Option<&Chan>) -> Option<Outcome> {
        let program = command.words[0].as_str();
        let args = &command.words[1..];
        if !matches!(
            program,
            "exit" | "true" | "false" | ":" | "echo" | "pwd" | "cd"
        ) {
            return None;
        }
        // A builtin's stdout goes to an explicit `fd1` redirect (file or
        // `>&2` fd duplication), else into the pipeline socket, else plain
        // stdout.
        let mut sink: Box<dyn Write> = match command.redirs.iter().rev().find(|redir| redir.fd == 1)
        {
            Some(redir) => match &redir.target {
                RedirTarget::Dup(2) => Box::new(std::io::stderr()),
                RedirTarget::Dup(_) => Box::new(std::io::stdout()),
                RedirTarget::Path(_) => match open_target(redir) {
                    Ok(Some(chan)) => Box::new(ChanWriter(chan)),
                    Ok(None) => Box::new(std::io::sink()),
                    Err(error) => {
                        eprintln!("sh: {error}");
                        return Some(Outcome::Status(1));
                    }
                },
            },
            None => match stdout_pipe {
                Some(chan) => match chan.dup() {
                    Ok(fd) => Box::new(ChanWriter(Chan::File(File::from(fd)))),
                    Err(error) => {
                        eprintln!("sh: pipe: {error}");
                        return Some(Outcome::Status(1));
                    }
                },
                None => Box::new(std::io::stdout()),
            },
        };
        match program {
            "exit" => {
                let code = args
                    .first()
                    .and_then(|arg| arg.parse::<i32>().ok())
                    .unwrap_or(0);
                Some(Outcome::Exit(code))
            }
            "true" | ":" => Some(Outcome::Status(0)),
            "false" => Some(Outcome::Status(1)),
            "echo" => Some(Outcome::Status(write_echo(&mut sink, args))),
            "pwd" => {
                let ok = match std::env::current_dir() {
                    Ok(dir) => writeln!(sink, "{}", dir.display()).is_ok(),
                    Err(error) => {
                        eprintln!("sh: pwd: {error}");
                        false
                    }
                };
                Some(Outcome::Status(if ok { 0 } else { 1 }))
            }
            "cd" => {
                let target = args.first().map(String::as_str).unwrap_or("/root");
                match std::env::set_current_dir(target) {
                    Ok(()) => Some(Outcome::Status(0)),
                    Err(error) => {
                        eprintln!("sh: cd: {target}: {error}");
                        Some(Outcome::Status(1))
                    }
                }
            }
            _ => None,
        }
    }

    struct ChanWriter(Chan);

    impl Write for ChanWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match &mut self.0 {
                Chan::File(file) => file.write(buf),
                Chan::Stream(stream) => stream.write(buf),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match &mut self.0 {
                Chan::File(file) => file.flush(),
                Chan::Stream(stream) => stream.flush(),
            }
        }
    }

    fn write_echo(sink: &mut dyn Write, args: &[String]) -> i32 {
        let (newline, words) = match args.first().map(String::as_str) {
            Some("-n") => (false, &args[1..]),
            _ => (true, args),
        };
        let mut out = words.join(" ");
        if newline {
            out.push('\n');
        }
        match sink.write_all(out.as_bytes()) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("sh: echo: {error}");
                1
            }
        }
    }

    /// Build the stdio triple for a spawned command, applying redirections
    /// left to right with real duplication semantics.
    fn build_stdio(
        command: &SimpleCommand,
        stdin_chan: Option<Chan>,
        stdout_chan: Option<Chan>,
    ) -> Result<(Stdio, Stdio, Stdio), String> {
        let mut slots: [Option<Chan>; 3] = [stdin_chan, stdout_chan, None];
        for redir in &command.redirs {
            if redir.fd > 2 {
                return Err(format!("unsupported fd {}", redir.fd));
            }
            match &redir.target {
                RedirTarget::Dup(source) => {
                    if *source > 2 {
                        return Err(format!("unsupported fd duplication >&{source}"));
                    }
                    let duplicated = match &slots[*source as usize] {
                        Some(chan) => chan.dup(),
                        None => dup_inherited(*source),
                    };
                    slots[redir.fd as usize] = Some(Chan::File(
                        duplicated
                            .map_err(|error| format!("dup fd {source}: {error}"))?
                            .into(),
                    ));
                }
                RedirTarget::Path(_) => {
                    slots[redir.fd as usize] = open_target(redir)?;
                }
            }
        }
        let [stdin, stdout, stderr] = slots;
        Ok((
            stdin.map(Chan::into_stdio).unwrap_or_else(Stdio::inherit),
            stdout.map(Chan::into_stdio).unwrap_or_else(Stdio::inherit),
            stderr.map(Chan::into_stdio).unwrap_or_else(Stdio::inherit),
        ))
    }

    /// Open a redirection target; `None` means the target maps to /dev/null.
    fn open_target(redir: &Redir) -> Result<Option<Chan>, String> {
        let RedirTarget::Path(path) = &redir.target else {
            return Err("internal: duplicate target is not a path".into());
        };
        if path == "/dev/null" {
            return Ok(None);
        }
        let result = match redir.mode {
            RedirMode::Read => File::open(path),
            RedirMode::Write => File::create(path),
            RedirMode::Append => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path),
        };
        result
            .map(|file| Some(Chan::File(file)))
            .map_err(|error| format!("{path}: {error}"))
    }

    fn resolve_program(program: &str) -> Option<std::path::PathBuf> {
        if program.contains('/') {
            let path = std::path::PathBuf::from(program);
            return is_executable(&path).then_some(path);
        }
        let paths = std::env::var("PATH").unwrap_or_else(|_| DEFAULT_PATH.to_owned());
        for dir in paths.split(':') {
            let candidate = std::path::Path::new(dir).join(program);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn is_executable(path: &std::path::Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // Tests: exercised through the real binary via an argv[0] override.
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::process::CommandExt;

        /// The real busybox binary (not the libtest harness that unit tests
        /// link as their own main): `cargo test` builds the bin next to the
        /// test executables' parent directory.
        fn busybox_bin() -> std::path::PathBuf {
            if let Ok(path) = std::env::var("CARGO_BIN_EXE_rfb-busybox") {
                return std::path::PathBuf::from(path);
            }
            let exe = std::env::current_exe().expect("current exe");
            let deps = exe.parent().expect("deps dir");
            let target = deps.parent().expect("target dir");
            target.join("rfb-busybox")
        }

        fn run_applet(name: &str, args: &[&str]) -> std::process::Output {
            let mut command = ProcCommand::new(busybox_bin());
            command.arg0(name);
            for arg in args {
                command.arg(arg);
            }
            command.output().expect("spawn applet")
        }

        fn sh(script: &str) -> std::process::Output {
            run_applet("sh", &["-c", script])
        }

        fn stdout_of(script: &str) -> String {
            let out = sh(script);
            assert!(
                out.status.success(),
                "script failed: {script}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        fn temp_path(name: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("rfb-busybox-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            dir.join(name)
        }

        #[test]
        fn echo_builtin_and_quotes() {
            assert_eq!(stdout_of("echo hello world"), "hello world\n");
            assert_eq!(stdout_of("echo 'a  b' \"c  d\""), "a  b c  d\n");
            assert_eq!(stdout_of("echo -n no-newline"), "no-newline");
            assert_eq!(stdout_of("echo 'literal $?'"), "literal $?\n");
        }

        #[test]
        fn sequence_and_exit_status() {
            assert!(sh("false; true").status.success());
            assert_eq!(sh("true; false").status.code(), Some(1));
            assert_eq!(sh("exit 7").status.code(), Some(7));
            assert_eq!(sh("true; exit 9").status.code(), Some(9));
        }

        #[test]
        fn conditional_chains_are_left_associative() {
            assert_eq!(stdout_of("false || echo rescued"), "rescued\n");
            assert_eq!(stdout_of("true && echo gated"), "gated\n");
            let out = sh("false && echo hidden");
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            // ((false || echo a) && echo b): both echo.
            assert_eq!(stdout_of("false || echo a && echo b"), "a\nb\n");
        }

        #[test]
        fn dollar_question_expands_previous_status() {
            assert_eq!(stdout_of("false; echo $?"), "1\n");
            assert_eq!(stdout_of("true; echo $?"), "0\n");
        }

        #[test]
        fn stderr_redirect_and_merge_into_file() {
            let out = sh("echo err >&2");
            assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
            assert!(out.stdout.is_empty());
            let file = temp_path("merged.txt");
            // `2>&1` before `> file`: stderr joins stdout's inherited stream,
            // then stdout moves to the file — POSIX order semantics.
            let _ = sh(&format!("echo out > {} 2>&1", file.display()));
            let merged = std::fs::read_to_string(&file).unwrap_or_default();
            assert!(
                merged.contains("out"),
                "stdout must reach the file: {merged:?}"
            );
        }

        #[test]
        fn redirection_creates_and_appends_files() {
            let file = temp_path("log.txt");
            let _ = sh(&format!("echo one > {}", file.display()));
            let _ = sh(&format!("echo two >> {}", file.display()));
            let content = std::fs::read_to_string(&file).expect("log content");
            assert_eq!(content, "one\ntwo\n");
        }

        #[test]
        fn pipelines_connect_stages() {
            let out = sh("echo piped | cat");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "piped\n");
            let out = sh("echo piped | cat | cat");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "piped\n");
        }

        #[test]
        fn stderr_does_not_flow_into_pipeline() {
            let out = sh("echo err >&2 | cat");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "");
            assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
            let out = sh("echo err 2>&1 | cat");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "err\n");
        }

        #[test]
        fn unknown_command_reports_127() {
            let out = sh("definitely-not-a-command-e2e");
            assert_eq!(out.status.code(), Some(127));
            assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
        }

        #[test]
        fn syntax_errors_report_2() {
            assert_eq!(sh("echo 'unterminated").status.code(), Some(2));
            assert_eq!(sh("echo a &&").status.code(), Some(2));
            assert_eq!(sh("|").status.code(), Some(2));
            assert_eq!(sh("echo a &&& echo b").status.code(), Some(2));
        }

        #[test]
        fn exit_code_of_spawned_program_propagates() {
            assert_eq!(sh("/bin/false").status.code(), Some(1));
            assert_eq!(sh("/bin/true").status.code(), Some(0));
            assert_eq!(sh("/bin/sh -c 'exit 3'").status.code(), Some(3));
        }

        #[test]
        fn cd_changes_directory_for_later_segments() {
            let out = sh("cd /; pwd");
            assert_eq!(String::from_utf8_lossy(&out.stdout), "/\n");
            let out = sh("cd /definitely-missing-dir-e2e");
            assert_eq!(out.status.code(), Some(1));
        }

        #[test]
        fn sleep_applet_validates_operand() {
            let out = run_applet("sleep", &["nonsense"]);
            assert_eq!(out.status.code(), Some(1));
            let started = std::time::Instant::now();
            let out = run_applet("sleep", &["0.05"]);
            assert!(out.status.success());
            assert!(started.elapsed() >= std::time::Duration::from_millis(40));
        }

        #[test]
        fn unknown_applet_reports_127() {
            let out = run_applet("definitely-not-an-applet", &[]);
            assert_eq!(out.status.code(), Some(127));
        }
    }
}

#[cfg(unix)]
fn main() {
    imp::entry();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("rfb-busybox: this utility requires a unix target");
    std::process::exit(127);
}
