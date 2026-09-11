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
