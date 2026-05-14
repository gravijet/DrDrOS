//! Pipeline = tokenizer + parser + executor for DrDrShell Tier 2.
//!
//! Three responsibilities, kept in one module because they share data:
//!
//!   1. [`tokenize`] — split a command line into Words and Operators,
//!      respecting single- and double-quoted strings.
//!   2. [`parse`]    — fold the token stream into a [`Pipeline`] of
//!      [`Stage`]s, each carrying its own argv + redirects.
//!   3. [`Pipeline::execute`] — spawn the stages, wire their stdio with
//!      [`Stdio::piped`] / file redirects, wait for every child, return
//!      the FINAL stage's exit code (matches sh / bash behaviour).
//!
//! Built-ins are *not* handled here — the caller of [`parse`] inspects the
//! result first and dispatches built-ins directly when the pipeline is a
//! single stage with no redirects. Built-ins inside a pipe would need
//! fork-and-write logic we don't tackle in Tier 2.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

// ─── Tokens ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Word(String),
    /// `|`
    Pipe,
    /// `<`
    RedirIn,
    /// `>`
    RedirOut,
    /// `>>`
    RedirAppend,
    /// `2>`
    RedirErr,
    /// `2>>`
    RedirErrAppend,
}

/// Tokenizer. Handles:
///   - whitespace as token separator
///   - "..." double quotes — keep contents verbatim, but allow `\"` escape
///   - '...' single quotes — keep contents fully verbatim (no escapes)
///   - the operators `|`, `>`, `>>`, `<`, `2>`, `2>>`
///
/// Errors come back as a human-readable string the caller can print.
pub fn tokenize(line: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    let mut buf = String::new();

    // Helper to flush the current word buffer, if any, as a Word token.
    macro_rules! flush_word {
        () => {
            if !buf.is_empty() {
                tokens.push(Token::Word(std::mem::take(&mut buf)));
            }
        };
    }

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                flush_word!();
            }
            '|' => {
                flush_word!();
                tokens.push(Token::Pipe);
            }
            '<' => {
                flush_word!();
                tokens.push(Token::RedirIn);
            }
            '>' => {
                flush_word!();
                if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push(Token::RedirAppend);
                } else {
                    tokens.push(Token::RedirOut);
                }
            }
            // `2>` and `2>>` only count when "2" is the start of a fresh
            // token AND the next character is '>'. Otherwise it's just a
            // digit in a word ("file2", "2nd").
            '2' if buf.is_empty() && chars.peek() == Some(&'>') => {
                chars.next(); // consume '>'
                if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push(Token::RedirErrAppend);
                } else {
                    tokens.push(Token::RedirErr);
                }
            }
            '"' => {
                // Double-quoted: copy chars verbatim until the matching ".
                // Permit `\"` and `\\` escapes; everything else passes
                // through (so `"$HOME"` stays the literal text — variable
                // expansion lands in Tier 3).
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(esc @ ('"' | '\\')) => buf.push(esc),
                            Some(other) => {
                                buf.push('\\');
                                buf.push(other);
                            }
                            None => return Err("unterminated escape in \"...\"".into()),
                        },
                        Some(other) => buf.push(other),
                        None => return Err("unterminated \"...\" string".into()),
                    }
                }
            }
            '\'' => {
                // Single-quoted: byte-for-byte literal, no escapes.
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(other) => buf.push(other),
                        None => return Err("unterminated '...' string".into()),
                    }
                }
            }
            other => buf.push(other),
        }
    }
    flush_word!();
    Ok(tokens)
}

// ─── Pipeline AST ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct Stage {
    pub program: String,
    pub args: Vec<String>,
    pub stdin_from: Option<PathBuf>,
    pub stdout_to: Option<RedirTarget>,
    pub stderr_to: Option<RedirTarget>,
}

#[derive(Debug, Clone)]
pub struct RedirTarget {
    pub path: PathBuf,
    pub append: bool,
}

#[derive(Debug, Default)]
pub struct Pipeline {
    pub stages: Vec<Stage>,
}

impl Pipeline {
    /// Convenience: a pipeline is "simple" when it has exactly one stage
    /// and no redirects. That's the only shape the caller can dispatch to
    /// a built-in.
    pub fn is_simple(&self) -> bool {
        self.stages.len() == 1
            && self.stages[0].stdin_from.is_none()
            && self.stages[0].stdout_to.is_none()
            && self.stages[0].stderr_to.is_none()
    }
}

/// Fold a token stream into a Pipeline. Each `Pipe` token starts a new
/// stage; redirect tokens attach the *next* Word to the current stage's
/// stdin / stdout / stderr slot.
pub fn parse(tokens: Vec<Token>) -> Result<Pipeline, String> {
    let mut pipeline = Pipeline::default();
    let mut stage = Stage::default();
    let mut iter = tokens.into_iter().peekable();

    while let Some(tok) = iter.next() {
        match tok {
            Token::Word(w) => {
                if stage.program.is_empty() {
                    stage.program = w;
                } else {
                    stage.args.push(w);
                }
            }
            Token::Pipe => {
                if stage.program.is_empty() {
                    return Err("syntax error near '|' (empty stage)".into());
                }
                if stage.stdout_to.is_some() {
                    return Err("cannot redirect stdout and pipe in the same stage".into());
                }
                pipeline.stages.push(std::mem::take(&mut stage));
            }
            Token::RedirIn => {
                let path = expect_word_after(&mut iter, "<")?;
                if stage.stdin_from.is_some() {
                    return Err("multiple '<' redirects".into());
                }
                stage.stdin_from = Some(PathBuf::from(path));
            }
            Token::RedirOut | Token::RedirAppend => {
                let append = matches!(tok, Token::RedirAppend);
                let path = expect_word_after(&mut iter, if append { ">>" } else { ">" })?;
                if stage.stdout_to.is_some() {
                    return Err("multiple stdout redirects".into());
                }
                stage.stdout_to = Some(RedirTarget { path: path.into(), append });
            }
            Token::RedirErr | Token::RedirErrAppend => {
                let append = matches!(tok, Token::RedirErrAppend);
                let path = expect_word_after(&mut iter, if append { "2>>" } else { "2>" })?;
                if stage.stderr_to.is_some() {
                    return Err("multiple stderr redirects".into());
                }
                stage.stderr_to = Some(RedirTarget { path: path.into(), append });
            }
        }
    }

    if stage.program.is_empty() && pipeline.stages.is_empty() {
        // Empty input — caller should treat the line as a no-op.
        return Ok(pipeline);
    }
    if stage.program.is_empty() {
        return Err("syntax error: pipeline ends with '|'".into());
    }
    pipeline.stages.push(stage);
    Ok(pipeline)
}

fn expect_word_after<I>(iter: &mut std::iter::Peekable<I>, op: &str) -> Result<String, String>
where
    I: Iterator<Item = Token>,
{
    match iter.next() {
        Some(Token::Word(w)) => Ok(w),
        Some(other) => Err(format!("expected filename after '{op}', got {other:?}")),
        None => Err(format!("expected filename after '{op}'")),
    }
}

// ─── Execution ───────────────────────────────────────────────────────

impl Pipeline {
    /// Spawn the pipeline, wait for every stage, return the FINAL stage's
    /// exit code. Errors (failed redirect open, missing executable, etc.)
    /// surface as Err and the caller decides how to display them.
    pub fn execute(self) -> Result<i32, String> {
        if self.stages.is_empty() {
            return Ok(0);
        }

        let n = self.stages.len();
        let mut children: Vec<Child> = Vec::with_capacity(n);
        let mut prev_stdout: Option<std::process::ChildStdout> = None;

        for (i, stage) in self.stages.into_iter().enumerate() {
            let is_last = i + 1 == n;

            let mut cmd = Command::new(&stage.program);
            cmd.args(&stage.args);

            // stdin: explicit file > pipe-from-previous > inherit.
            if let Some(path) = &stage.stdin_from {
                let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
                cmd.stdin(Stdio::from(f));
            } else if let Some(prev) = prev_stdout.take() {
                cmd.stdin(Stdio::from(prev));
            } // else inherit parent's stdin.

            // stdout: explicit file > piped (if not last) > inherit.
            if let Some(target) = &stage.stdout_to {
                let f = open_redirect(target)?;
                cmd.stdout(Stdio::from(f));
            } else if !is_last {
                cmd.stdout(Stdio::piped());
            }

            // stderr: only file redirect or inherit. We don't pipe stderr.
            if let Some(target) = &stage.stderr_to {
                let f = open_redirect(target)?;
                cmd.stderr(Stdio::from(f));
            }

            let mut child = cmd.spawn().map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => format!("{}: command not found", stage.program),
                _ => format!("{}: {e}", stage.program),
            })?;

            // Capture this child's stdout if a later stage needs it.
            prev_stdout = child.stdout.take();
            children.push(child);
        }

        // Wait on every child. The final child's exit code is what the
        // shell reports — same convention as POSIX sh.
        let mut final_status: i32 = 0;
        for (i, mut child) in children.into_iter().enumerate() {
            let status = child.wait().map_err(|e| format!("wait: {e}"))?;
            if i + 1 == n {
                final_status = status_to_code(status);
            }
        }
        Ok(final_status)
    }
}

fn open_redirect(target: &RedirTarget) -> Result<File, String> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true);
    if target.append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    opts.open(&target.path).map_err(|e| format!("{}: {e}", target.path.display()))
}

fn status_to_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        code
    } else {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    }
}
