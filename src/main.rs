use anyhow::{Context, Result};
use stale_urls::{
    BOLD_RED, CYAN, CheckOutcome, CheckProgress, DIM, FoundUrl, GREEN, RED, ScanResult, Styled,
    check_urls_with_phases, scan, scan_with_counted, styled,
};
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use terminal_size::{Width, terminal_size_of};

const DEFAULT_TERMINAL_WIDTH: usize = 80;

fn main() -> Result<ExitCode> {
    let stdout_color = color_enabled(io::stdout().is_terminal());
    let stderr_color = color_enabled(io::stderr().is_terminal());
    let scan = scan_current_repository()?;
    let urls_found: usize = scan.urls.iter().map(|found| found.occurrences.len()).sum();
    let unique_urls = scan.urls.len();
    println!("Found {urls_found} GitHub URL(s), {unique_urls} unique.");
    let file_errors = scan.errors.len();
    let mut url_errors = 0;

    for error in &scan.errors {
        eprintln!("{} {error}", styled("error:", BOLD_RED, stderr_color));
    }

    let outcomes = check_urls_with_progress(scan.urls)?;
    let mut current = 0;
    let mut stale = 0;

    for outcome in outcomes {
        match outcome {
            CheckOutcome::Current { .. } => current += 1,
            CheckOutcome::Stale(report) => {
                stale += 1;
                if stdout_color {
                    println!("{}", report.colored());
                } else {
                    println!("{report}");
                }
            }
            CheckOutcome::Error { found, message } => {
                url_errors += 1;
                eprintln!(
                    "{} {}",
                    styled("error:", BOLD_RED, stderr_color),
                    styled(&found.url.to_string(), CYAN, stderr_color)
                );
                for occurrence in found.occurrences {
                    eprintln!(
                        "  {} {}:{}",
                        styled("at", DIM, stderr_color),
                        occurrence.file.display(),
                        occurrence.line
                    );
                }
                eprintln!("  {message}");
            }
        }
    }

    assert_eq!(unique_urls, current + stale + url_errors);
    println!(
        "Checked {} unique URL(s): {}, {}, {}; {}.",
        unique_urls,
        styled_count_with_label(current, "current", GREEN, stdout_color),
        styled_count_with_label(stale, "stale", RED, stdout_color),
        styled_count_with_label(url_errors, "URL error(s)", RED, stdout_color),
        styled_count_with_label(file_errors, "file error(s)", RED, stdout_color),
    );

    if stale != 0 || url_errors != 0 || file_errors != 0 {
        return Ok(ExitCode::FAILURE);
    }

    Ok(ExitCode::SUCCESS)
}

fn scan_current_repository() -> Result<ScanResult> {
    let mut stderr_lock = io::stderr().lock();
    if !stderr_lock.is_terminal() {
        return scan(".").context("could not scan the current repository");
    }

    let color = color_enabled(true);
    let result = scan_with_counted(".", |path, index, total| {
        let path_text = path.to_string_lossy();
        let path_text: String = path_text.chars().flat_map(char::escape_debug).collect();
        let suffix = format!(" ({index}/{total})");
        let _ = write_progress(
            &mut stderr_lock,
            "Scanning: ",
            &path_text,
            &suffix,
            None,
            color,
        );
    });
    clear_progress(&mut stderr_lock)?;
    result.context("could not scan the current repository")
}

fn check_urls_with_progress(urls: Vec<FoundUrl>) -> Result<Vec<CheckOutcome>> {
    let mut stderr_lock = io::stderr().lock();
    let interactive = stderr_lock.is_terminal();
    let color = color_enabled(interactive);
    let outcomes = check_urls_with_phases(urls, |progress| {
        let (label, value, index, total, value_style) = match progress {
            CheckProgress::Preparing {
                repository,
                index,
                total,
            } => ("Preparing: ", repository.to_owned(), index, total, None),
            CheckProgress::Checking { url, index, total } => (
                "Checking: ",
                url.text.chars().flat_map(char::escape_debug).collect(),
                index,
                total,
                Some(CYAN),
            ),
            CheckProgress::InvestigationCount { total } => {
                if interactive {
                    let _ = clear_progress(&mut stderr_lock);
                }
                println!("{total} URL(s) require investigation.");
                return;
            }
            CheckProgress::Investigating { url, index, total } => (
                "Investigating: ",
                url.text.chars().flat_map(char::escape_debug).collect(),
                index,
                total,
                Some(CYAN),
            ),
        };
        let suffix = format!(" ({index}/{total})");
        if interactive {
            let _ = write_progress(&mut stderr_lock, label, &value, &suffix, value_style, color);
        }
    });
    if interactive {
        clear_progress(&mut stderr_lock)?;
    }
    Ok(outcomes)
}

fn write_progress(
    output: &mut impl Write,
    label: &str,
    value: &str,
    suffix: &str,
    value_style: Option<&str>,
    color: bool,
) -> io::Result<()> {
    let available = terminal_width().saturating_sub(label.len() + suffix.chars().count() + 1);
    let value = truncate(value, available);
    write!(output, "\r\x1b[2K")?;
    write!(
        output,
        "{}{}{suffix}",
        styled(label, DIM, color),
        styled(
            &value,
            value_style.unwrap_or_default(),
            color && value_style.is_some()
        ),
    )?;
    output.flush()
}

fn clear_progress(output: &mut impl Write) -> io::Result<()> {
    write!(output, "\r\x1b[2K")?;
    output.flush()
}

fn terminal_width() -> usize {
    terminal_size_of(io::stderr())
        .map(|(Width(width), _)| usize::from(width))
        .filter(|width| *width != 0)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(DEFAULT_TERMINAL_WIDTH)
}

fn color_enabled(is_terminal: bool) -> bool {
    is_terminal && std::env::var_os("NO_COLOR").is_none()
}

fn styled_count_with_label<'a>(
    value: usize,
    label: &str,
    nonzero_style: &'a str,
    color: bool,
) -> Styled<'a, String> {
    styled(
        format!("{value} {label}"),
        if value == 0 { DIM } else { nonzero_style },
        color,
    )
}

fn truncate(value: &str, width: usize) -> String {
    let length = value.chars().count();
    if length <= width {
        return value.to_owned();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let start: String = value.chars().take(width - 3).collect();
    format!("{start}...")
}

#[cfg(test)]
mod tests {
    use super::{RED, styled, truncate, write_progress};

    #[test]
    fn truncates_the_end_to_preserve_the_start_of_the_path() {
        assert_eq!(truncate("a/long/path/file.rs", 12), "a/long/pa...");
        assert_eq!(truncate("short.rs", 12), "short.rs");
    }

    #[test]
    fn styling_can_be_disabled() {
        assert_eq!(styled("stale", RED, false).to_string(), "stale");
        assert_eq!(
            styled("stale", RED, true).to_string(),
            "\x1b[31mstale\x1b[0m"
        );
    }

    #[test]
    fn progress_keeps_the_counter_suffix() {
        let mut output = Vec::new();
        write_progress(
            &mut output,
            "Checking: ",
            "https://example.com",
            " (2/3)",
            Some(super::CYAN),
            false,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\r\x1b[2KChecking: https://example.com (2/3)"
        );

        let mut output = Vec::new();
        write_progress(
            &mut output,
            "Checking: ",
            "https://example.com",
            " (2/3)",
            Some(super::CYAN),
            true,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\r\x1b[2K\x1b[2mChecking: \x1b[0m\x1b[36mhttps://example.com\x1b[0m (2/3)"
        );
    }
}
