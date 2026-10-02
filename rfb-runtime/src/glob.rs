//! Glob name matching — the single find-pattern semantics shared by every
//! find implementation (forkd agent search, ZBRT workspace executor): `*`
//! matches any sequence (including separators), all other characters are
//! literal, full-name match.

/// Match `text` against a glob `pattern` (currently: one trailing-ish `*`).
///
/// # Examples
///
/// ```ignore
/// assert!(glob_matches("*.txt", "note.txt"));
/// assert!(!glob_matches("note", "note.txt"));
/// ```
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Bounded recursive name walk shared by the two find implementations (forkd
/// agent search and the ZBRT workspace executor): descends `dir` (reported
/// relative to `root`, `/`-separated), pushing every entry whose name matches
/// `matcher` until `max` matches. Returns whether the walk stopped early
/// because `max` was reached.
///
/// `skip_symlinks` is the workspace executor's cycle guard; the agent walk
/// matches symlink names like any other entry and does not follow them
/// (`file_type().is_dir()` is false for symlinks).
// agent search lives behind forkd (which implies guest in this crate); the
// moved agent_contract test crate only has rfb's forkd, hence the `any`.
#[cfg(any(feature = "guest", feature = "forkd"))]
pub(crate) fn bounded_name_walk(
    root: &std::path::Path,
    dir: &std::path::Path,
    matcher: &impl Fn(&str) -> bool,
    max: usize,
    skip_symlinks: bool,
    out: &mut Vec<String>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> std::io::Result<bool> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        // 取消检查（每目录项一次，原子读 ~ns）：取消一个大扫描不再要等它
        // 扫完整个 workspace。
        if let Some(flag) = cancel {
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("request cancelled"));
            }
        }
        let file_type = e.file_type()?;
        if skip_symlinks && file_type.is_symlink() {
            continue;
        }
        if out.len() >= max {
            return Ok(true);
        }
        let p = e.path();
        if matcher(&e.file_name().to_string_lossy()) {
            out.push(
                p.strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
        if file_type.is_dir()
            && bounded_name_walk(root, &p, matcher, max, skip_symlinks, out, cancel)?
        {
            return Ok(true);
        }
    }
    Ok(out.len() >= max)
}
