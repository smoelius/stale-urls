use anyhow::{Context, Error, Result, anyhow, bail};
use percent_encoding::percent_decode_str;
use regex::Regex;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt::{self, Write as _},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::LazyLock,
};

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const BOLD_RED: &str = "\x1b[1;31m";
pub const GREEN: &str = "\x1b[32m";
pub const BOLD_YELLOW: &str = "\x1b[1;33m";
pub const BOLD_MAGENTA: &str = "\x1b[1;35m";
pub const CYAN: &str = "\x1b[36m";

/// A value with optional ANSI styling.
pub struct Styled<'a, T> {
    value: T,
    style: &'a str,
    color: bool,
}

/// Apply an ANSI style when color is enabled.
pub fn styled<T: fmt::Display>(value: T, style: &str, color: bool) -> Styled<'_, T> {
    Styled {
        value,
        style,
        color,
    }
}

impl<T: fmt::Display> fmt::Display for Styled<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            value,
            style,
            color,
        } = self;
        if *color {
            write!(f, "{style}{value}{RESET}")
        } else {
            write!(f, "{value}")
        }
    }
}

static URL_CANDIDATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https://github\.com/[^\s<>"'`]+"#).expect("URL candidate regex must compile")
});

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^https://github\.com/",
        r"([A-Za-z0-9_.-]+)/",      // owner
        r"([A-Za-z0-9_.-]+)/blob/", // repository
        r"([0-9A-Fa-f]{40})/",      // full commit hash
        r"([^#]+)",                 // path
        r"#L([0-9]+)",              // first line
        r"(?:-L([0-9]+))?$",        // optional last line
    ))
    .expect("GitHub URL regex must compile")
});

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceUrl {
    pub text: String,
    pub owner: String,
    pub repo: String,
    pub commit: String,
    pub path: String,
    pub start: usize,
    pub end: usize,
}

impl fmt::Display for SourceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Occurrence {
    pub file: PathBuf,
    pub line: usize,
}

#[derive(Clone, Debug)]
pub struct FoundUrl {
    pub url: SourceUrl,
    pub occurrences: Vec<Occurrence>,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub urls: Vec<FoundUrl>,
    pub errors: Vec<String>,
}

pub fn scan(root: impl AsRef<Path>) -> Result<ScanResult> {
    scan_with(root, |_| {})
}

pub fn scan_with(root: impl AsRef<Path>, mut on_file: impl FnMut(&Path)) -> Result<ScanResult> {
    scan_with_counted(root, |path, _, _| on_file(path))
}

pub fn scan_with_counted(
    root: impl AsRef<Path>,
    mut on_file: impl FnMut(&Path, usize, usize),
) -> Result<ScanResult> {
    let root = root.as_ref();
    let mut found: BTreeMap<SourceUrl, Vec<Occurrence>> = BTreeMap::new();
    let mut errors = Vec::new();

    let output = git_output(root, ["ls-files", "-z"])?;
    if !output.status.success() {
        return Err(git_failure("list tracked files", &output));
    }

    let paths: Vec<PathBuf> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(path_from_git)
        .collect::<Result<_>>()?;
    let total = paths.len();

    for (index, relative_path) in paths.into_iter().enumerate() {
        let path_buf = root.join(&relative_path);
        on_file(&relative_path, index + 1, total);

        let bytes = match read_tracked_path(&path_buf) {
            Ok(bytes) => bytes,
            Err(error) => {
                errors.push(format!("{}: {error}", path_buf.display()));
                continue;
            }
        };
        let Ok(contents) = std::str::from_utf8(&bytes) else {
            continue;
        };

        for candidate in URL_CANDIDATE_RE.find_iter(contents) {
            let Some(url) = parse_candidate(candidate.as_str()) else {
                continue;
            };
            let offset = candidate.start();
            let line = 1 + contents.as_bytes()[..offset]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count();
            found.entry(url).or_default().push(Occurrence {
                file: path_buf.clone(),
                line,
            });
        }
    }

    Ok(ScanResult {
        urls: found
            .into_iter()
            .map(|(url, occurrences)| FoundUrl { url, occurrences })
            .collect(),
        errors,
    })
}

fn read_tracked_path(path: &Path) -> std::io::Result<Vec<u8>> {
    if !fs::symlink_metadata(path)?.file_type().is_symlink() {
        return fs::read(path);
    }

    let target = fs::read_link(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    // smoelius: This code is currently unused as we rely on `xdg`, a Unix-specific dependency.
    #[cfg(not(unix))]
    {
        target
            .into_os_string()
            .into_string()
            .map(String::into_bytes)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "symlink target is not valid UTF-8",
                )
            })
    }
}

fn path_from_git(path: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(path.to_vec()).into())
    }
    // smoelius: This code is currently unused as we rely on `xdg`, a Unix-specific dependency.
    #[cfg(not(unix))]
    {
        Ok(String::from_utf8(path.to_vec())
            .context("Git returned a non-UTF-8 path")?
            .into())
    }
}

fn parse_candidate(candidate: &str) -> Option<SourceUrl> {
    let mut candidate = candidate;
    while let Some(last) = candidate.as_bytes().last()
        && matches!(last, b')' | b']' | b'}' | b',' | b'.' | b';' | b':')
    {
        candidate = &candidate[..candidate.len() - 1];
    }
    let captures = URL_RE.captures(candidate)?;
    if matches!(&captures[1], "." | "..") || matches!(&captures[2], "." | "..") {
        return None;
    }
    let path = percent_decode_str(&captures[4])
        .decode_utf8()
        .ok()?
        .into_owned();
    let start = captures[5].parse().ok()?;
    let end = captures
        .get(6)
        .map_or(Ok(start), |value| value.as_str().parse())
        .ok()?;

    Some(SourceUrl {
        text: candidate.to_owned(),
        owner: captures[1].to_owned(),
        repo: captures[2].to_owned(),
        commit: captures[3].to_ascii_lowercase(),
        path,
        start,
        end,
    })
}

#[derive(Debug)]
pub enum CheckOutcome {
    Current { url: SourceUrl },
    Stale(StaleReport),
    Error { found: FoundUrl, message: String },
}

#[derive(Debug)]
pub struct CommitInfo {
    pub hash: String,
    pub date: String,
    pub title: String,
}

#[derive(Debug)]
pub struct StaleReport {
    pub found: FoundUrl,
    pub change_commit: CommitInfo,
    pub merge_commit: CommitInfo,
}

pub struct ColoredStaleReport<'a>(&'a StaleReport);

impl StaleReport {
    pub fn colored(&self) -> ColoredStaleReport<'_> {
        ColoredStaleReport(self)
    }
}

impl fmt::Display for StaleReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_report(f, self, false)
    }
}

impl fmt::Display for ColoredStaleReport<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_report(f, self.0, true)
    }
}

fn write_report(f: &mut fmt::Formatter<'_>, report: &StaleReport, color: bool) -> fmt::Result {
    write_styled(f, color, BOLD_RED, format_args!("Stale URL:"))?;
    write!(f, " ")?;
    write_styled(f, color, CYAN, format_args!("{}", report.found.url))?;
    writeln!(f)?;
    write_styled(f, color, DIM, format_args!("Found at:"))?;
    writeln!(f)?;
    for occurrence in &report.found.occurrences {
        writeln!(f, "  {}:{}", occurrence.file.display(), occurrence.line)?;
    }
    write_commit(
        f,
        "Change commit",
        BOLD_YELLOW,
        &report.change_commit,
        &report.found.url,
        color,
    )?;
    if report.change_commit.hash != report.merge_commit.hash {
        write_commit(
            f,
            "Merge commit",
            BOLD_MAGENTA,
            &report.merge_commit,
            &report.found.url,
            color,
        )?;
    }
    Ok(())
}

fn write_styled(
    f: &mut fmt::Formatter<'_>,
    color: bool,
    style: &str,
    value: fmt::Arguments<'_>,
) -> fmt::Result {
    write!(f, "{}", styled(value, style, color))
}

fn write_field(f: &mut fmt::Formatter<'_>, label: &str, color: bool) -> fmt::Result {
    write_styled(f, color, DIM, format_args!("  {label}"))
}

fn write_commit(
    f: &mut fmt::Formatter<'_>,
    label: &str,
    label_style: &str,
    commit: &CommitInfo,
    url: &SourceUrl,
    color: bool,
) -> fmt::Result {
    write_styled(f, color, label_style, format_args!("{label}:"))?;
    writeln!(f)?;
    write_field(f, "Commit: ", color)?;
    writeln!(f, "{}", commit.hash)?;
    write_field(f, "Date:   ", color)?;
    writeln!(f, "{}", commit.date)?;
    write_field(f, "Title:  ", color)?;
    writeln!(f, "{}", commit.title)?;
    write_field(f, "URL:    ", color)?;
    write_styled(
        f,
        color,
        CYAN,
        format_args!(
            "https://github.com/{}/{}/commit/{}",
            url.owner, url.repo, commit.hash
        ),
    )?;
    writeln!(f)
}

pub fn check_urls(urls: Vec<FoundUrl>) -> Vec<CheckOutcome> {
    check_urls_with(urls, |_, _, _| {})
}

pub fn check_urls_with(
    urls: Vec<FoundUrl>,
    mut on_url: impl FnMut(&SourceUrl, usize, usize),
) -> Vec<CheckOutcome> {
    check_urls_with_phases(urls, |progress| {
        if let CheckProgress::Checking { url, index, total } = progress {
            on_url(url, index, total);
        }
    })
}

pub enum CheckProgress<'a> {
    Preparing {
        repository: &'a str,
        index: usize,
        total: usize,
    },
    Checking {
        url: &'a SourceUrl,
        index: usize,
        total: usize,
    },
    InvestigationCount {
        total: usize,
    },
    Investigating {
        url: &'a SourceUrl,
        index: usize,
        total: usize,
    },
}

type RepositoryKey = (String, String);

struct Candidate {
    key: RepositoryKey,
    found: FoundUrl,
    expected: Vec<Vec<u8>>,
}

pub fn check_urls_with_phases(
    urls: Vec<FoundUrl>,
    on_progress: impl FnMut(CheckProgress<'_>),
) -> Vec<CheckOutcome> {
    let cache = match cache_directory() {
        Ok(cache) => cache,
        Err(error) => {
            return urls
                .into_iter()
                .map(|found| CheckOutcome::Error {
                    found,
                    message: format!("could not initialize the user cache: {error:#}"),
                })
                .collect();
        }
    };

    check_urls_in_cache(&cache, urls, on_progress)
}

fn check_urls_in_cache(
    cache: &Path,
    urls: Vec<FoundUrl>,
    mut on_progress: impl FnMut(CheckProgress<'_>),
) -> Vec<CheckOutcome> {
    let mut groups: BTreeMap<RepositoryKey, Vec<FoundUrl>> = BTreeMap::new();
    for found in urls {
        groups
            .entry((found.url.owner.clone(), found.url.repo.clone()))
            .or_default()
            .push(found);
    }

    let mut repositories = BTreeMap::new();
    let mut prepared = Vec::new();
    let mut outcomes = Vec::new();
    let repository_total = groups.len();

    for (index, (key, group)) in groups.into_iter().enumerate() {
        let repository_name = format!("{}/{}", key.0, key.1);
        on_progress(CheckProgress::Preparing {
            repository: &repository_name,
            index: index + 1,
            total: repository_total,
        });
        let repository = match Repository::open_or_update(cache, &group[0].url) {
            Ok(repository) => repository,
            Err(error) => {
                let message = format!("{error:#}");
                outcomes.extend(group.into_iter().map(|found| CheckOutcome::Error {
                    found,
                    message: message.clone(),
                }));
                continue;
            }
        };

        let commits: Vec<&str> = group
            .iter()
            .map(|found| found.url.commit.as_str())
            .collect();
        if repository.fetch_commits(commits).is_ok() {
            prepared.extend(group.into_iter().map(|found| (key.clone(), found)));
        } else {
            // A single unavailable hash should not prevent other URLs from being checked.
            for found in group {
                match repository.fetch_commit(&found.url.commit) {
                    Ok(()) => prepared.push((key.clone(), found)),
                    Err(error) => outcomes.push(CheckOutcome::Error {
                        found,
                        message: format!("{error:#}"),
                    }),
                }
            }
        }
        repositories.insert(key, repository);
    }

    let check_total = prepared.len();
    let mut candidates = Vec::new();
    for (index, (key, found)) in prepared.into_iter().enumerate() {
        on_progress(CheckProgress::Checking {
            url: &found.url,
            index: index + 1,
            total: check_total,
        });
        let repository = repositories
            .get(&key)
            .expect("prepared URLs have a cached repository");
        match check_local(repository, &found) {
            Ok(Some(expected)) => candidates.push(Candidate {
                key,
                found,
                expected,
            }),
            Ok(None) => outcomes.push(CheckOutcome::Current { url: found.url }),
            Err(error) => outcomes.push(CheckOutcome::Error {
                found,
                message: format!("{error:#}"),
            }),
        }
    }

    let investigation_total = candidates.len();
    on_progress(CheckProgress::InvestigationCount {
        total: investigation_total,
    });
    let mut history_status: BTreeMap<RepositoryKey, Option<String>> = BTreeMap::new();
    for (index, candidate) in candidates.into_iter().enumerate() {
        on_progress(CheckProgress::Investigating {
            url: &candidate.found.url,
            index: index + 1,
            total: investigation_total,
        });
        let repository = repositories
            .get(&candidate.key)
            .expect("investigation candidates have a cached repository");
        let history_error = history_status
            .entry(candidate.key.clone())
            .or_insert_with(|| {
                repository
                    .ensure_full_default_history()
                    .err()
                    .map(|error| format!("{error:#}"))
            });
        if let Some(message) = history_error {
            outcomes.push(CheckOutcome::Error {
                found: candidate.found,
                message: message.clone(),
            });
            continue;
        }

        match investigate(repository, &candidate.found, &candidate.expected) {
            Ok(Some(report)) => outcomes.push(CheckOutcome::Stale(report)),
            Ok(None) => outcomes.push(CheckOutcome::Current {
                url: candidate.found.url,
            }),
            Err(error) => outcomes.push(CheckOutcome::Error {
                found: candidate.found,
                message: format!("{error:#}"),
            }),
        }
    }
    outcomes
}

fn cache_directory() -> Result<PathBuf> {
    let directories = xdg::BaseDirectories::new();
    directories
        .create_cache_directory("stale-urls")
        .context("could not create the stale-urls cache directory")
}

fn check_local(repository: &Repository, found: &FoundUrl) -> Result<Option<Vec<Vec<u8>>>> {
    if found.url.start == 0 || found.url.end < found.url.start {
        bail!("invalid line range L{}-L{}", found.url.start, found.url.end);
    }

    let referenced = repository
        .blob(&found.url.commit, &found.url.path)?
        .ok_or_else(|| anyhow!("{} does not exist at the referenced commit", found.url.path))?;
    let expected = extract_lines(&referenced, found.url.start, found.url.end)?;

    if repository.contains_at(&repository.default_ref, &found.url.path, &expected)? {
        return Ok(None);
    }

    Ok(Some(expected))
}

fn investigate(
    repository: &Repository,
    found: &FoundUrl,
    expected: &[Vec<u8>],
) -> Result<Option<StaleReport>> {
    if !repository.is_ancestor(&found.url.commit, &repository.default_ref)? {
        bail!(
            "referenced commit {} is not an ancestor of the current default branch",
            found.url.commit
        );
    }

    let Some(integration_hash) = repository.find_break(
        &found.url.commit,
        &repository.default_ref,
        &found.url.path,
        expected,
    )?
    else {
        // The initial current-path check can fail after an ordinary rename. History
        // tracking establishes that the exact lines still exist in the renamed file.
        return Ok(None);
    };

    let original_hash =
        repository.find_original_change(&integration_hash, &found.url.path, expected, 0)?;
    Ok(Some(StaleReport {
        found: found.clone(),
        change_commit: repository.commit_info(&original_hash)?,
        merge_commit: repository.commit_info(&integration_hash)?,
    }))
}

struct Repository {
    path: PathBuf,
    default_branch: String,
    default_ref: String,
}

impl Repository {
    fn open_or_update(cache: &Path, url: &SourceUrl) -> Result<Self> {
        let owner_directory = cache.join(&url.owner);
        let path_buf = owner_directory.join(&url.repo);
        fs::create_dir_all(&owner_directory)
            .with_context(|| format!("could not create {}", owner_directory.display()))?;

        if path_buf.exists() {
            if !path_buf.join(".git").is_dir() {
                bail!("cache path {} is not a Git repository", path_buf.display());
            }
        } else {
            let remote = format!("https://github.com/{}/{}.git", url.owner, url.repo);
            command_ok(
                Command::new("git")
                    .args(["clone", "--depth", "1", "--no-tags", "--quiet"])
                    .arg(&remote)
                    .arg(&path_buf),
                "shallow-clone repository",
            )?;
        }

        let default_branch = remote_default_branch(&path_buf)?;
        let default_ref = format!("refs/remotes/origin/{default_branch}");
        git_ok(
            &path_buf,
            [
                "fetch",
                "--quiet",
                "--prune",
                "origin",
                &format!("+refs/heads/{default_branch}:{default_ref}"),
            ],
            "update cached default branch",
        )?;

        let current_branch = git_text(&path_buf, ["branch", "--show-current"], "read branch")?;
        if current_branch.trim() == default_branch {
            git_ok(
                &path_buf,
                ["merge", "--quiet", "--ff-only", &default_ref],
                "fast-forward cached default branch",
            )?;
        } else {
            git_ok(
                &path_buf,
                ["checkout", "--quiet", "-B", &default_branch, &default_ref],
                "check out the repository's default branch",
            )?;
        }

        Ok(Self {
            path: path_buf,
            default_branch,
            default_ref,
        })
    }

    fn fetch_commit(&self, commit: &str) -> Result<()> {
        self.fetch_commits([commit])
    }

    fn fetch_commits<'a>(&self, commits: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let mut missing = Vec::new();
        for commit in commits {
            if !self.object_exists(&format!("{commit}^{{commit}}"))? {
                missing.push(commit.to_owned());
            }
        }
        missing.sort_unstable();
        missing.dedup();
        if missing.is_empty() {
            return Ok(());
        }

        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.path)
            .args(["fetch", "--quiet", "--depth=1", "--no-tags", "origin"])
            .args(missing);
        command_ok(&mut command, "fetch referenced commit")
    }

    fn ensure_full_default_history(&self) -> Result<()> {
        let shallow = git_text(
            &self.path,
            ["rev-parse", "--is-shallow-repository"],
            "inspect clone depth",
        )?;
        if shallow.trim() == "true" {
            git_ok(
                &self.path,
                [
                    "fetch",
                    "--quiet",
                    "--unshallow",
                    "origin",
                    &self.default_branch,
                ],
                "deepen default-branch history",
            )?;
        }
        Ok(())
    }

    fn object_exists(&self, object: &str) -> Result<bool> {
        let status = git_output(&self.path, ["cat-file", "-e", object])?.status;
        Ok(status.success())
    }

    fn blob(&self, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let spec = format!("{commit}:{path}");
        let output = git_output(&self.path, ["show", &spec])?;
        if output.status.success() {
            Ok(Some(output.stdout))
        } else {
            Ok(None)
        }
    }

    fn contains_at(&self, commit: &str, path: &str, expected: &[Vec<u8>]) -> Result<bool> {
        Ok(self
            .blob(commit, path)?
            .is_some_and(|blob| contains_lines(&blob, expected)))
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let output = git_output(
            &self.path,
            ["merge-base", "--is-ancestor", ancestor, descendant],
        )?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(git_failure("check commit ancestry", &output)),
        }
    }

    fn find_break(
        &self,
        start: &str,
        end: &str,
        initial_path: &str,
        expected: &[Vec<u8>],
    ) -> Result<Option<String>> {
        let range = format!("{start}..{end}");
        let commits = git_lines(
            &self.path,
            ["rev-list", "--first-parent", "--reverse", &range],
            "walk default-branch history",
        )?;
        let mut previous = start.to_owned();
        let mut path = initial_path.to_owned();

        for commit in commits {
            path = self.renamed_path(&previous, &commit, &path, true)?;
            if !self.contains_at(&commit, &path, expected)? {
                return Ok(Some(commit));
            }
            previous = commit;
        }
        Ok(None)
    }

    fn renamed_path(&self, old: &str, new: &str, path: &str, forward: bool) -> Result<String> {
        let output = git_output(&self.path, ["diff", "--name-status", "-z", "-M", old, new])?;
        if !output.status.success() {
            return Err(git_failure("detect file renames", &output));
        }
        let fields: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
        let mut index = 0;
        while index < fields.len() && !fields[index].is_empty() {
            let status = fields[index];
            index += 1;
            if status.first() == Some(&b'R') || status.first() == Some(&b'C') {
                if index + 1 >= fields.len() {
                    break;
                }
                let old_path = String::from_utf8_lossy(fields[index]);
                let new_path = String::from_utf8_lossy(fields[index + 1]);
                if (forward && old_path == path) || (!forward && new_path == path) {
                    return Ok(if forward {
                        new_path.into_owned()
                    } else {
                        old_path.into_owned()
                    });
                }
                index += 2;
            } else {
                index += 1;
            }
        }
        Ok(path.to_owned())
    }

    fn find_original_change(
        &self,
        integration: &str,
        path: &str,
        expected: &[Vec<u8>],
        depth: usize,
    ) -> Result<String> {
        if depth >= 16 {
            return Ok(integration.to_owned());
        }
        let parents = self.parents(integration)?;
        if parents.len() < 2 {
            return Ok(integration.to_owned());
        }

        let first_parent = &parents[0];
        for side_parent in &parents[1..] {
            let base = git_text(
                &self.path,
                ["merge-base", first_parent, side_parent],
                "find merge base",
            )?;
            let base = base.trim();
            if !self.contains_at(base, path, expected)? {
                continue;
            }
            if let Some(candidate) = self.find_break(base, side_parent, path, expected)? {
                return self.find_original_change(&candidate, path, expected, depth + 1);
            }
        }

        Ok(integration.to_owned())
    }

    fn parents(&self, commit: &str) -> Result<Vec<String>> {
        let line = git_text(
            &self.path,
            ["rev-list", "--parents", "-n", "1", commit],
            "read commit parents",
        )?;
        Ok(line.split_whitespace().skip(1).map(str::to_owned).collect())
    }

    fn commit_info(&self, commit: &str) -> Result<CommitInfo> {
        let text = git_text(
            &self.path,
            ["show", "-s", "--format=%H%x00%cI%x00%s", commit],
            "read commit metadata",
        )?;
        let mut fields = text.trim_end().splitn(3, '\0');
        Ok(CommitInfo {
            hash: fields.next().unwrap_or_default().to_owned(),
            date: fields.next().unwrap_or_default().to_owned(),
            title: fields.next().unwrap_or_default().to_owned(),
        })
    }
}

fn remote_default_branch(repository: &Path) -> Result<String> {
    let output = git_output(repository, ["ls-remote", "--symref", "origin", "HEAD"])?;
    if !output.status.success() {
        return Err(git_failure("determine remote default branch", &output));
    }
    let text = String::from_utf8(output.stdout).context("Git emitted non-UTF-8 output")?;
    text.lines()
        .find_map(|line| {
            line.strip_prefix("ref: refs/heads/")
                .and_then(|rest| rest.strip_suffix("\tHEAD"))
                .map(str::to_owned)
        })
        .ok_or_else(|| anyhow!("origin did not advertise a default branch"))
}

fn extract_lines(blob: &[u8], start: usize, end: usize) -> Result<Vec<Vec<u8>>> {
    let lines = lines(blob);
    if start == 0 || end < start || end > lines.len() {
        bail!(
            "referenced range L{start}-L{end} is outside a file containing {} line(s)",
            lines.len()
        );
    }
    Ok(lines[start - 1..end]
        .iter()
        .map(|line| line.to_vec())
        .collect())
}

fn contains_lines(blob: &[u8], expected: &[Vec<u8>]) -> bool {
    if expected.is_empty() {
        return false;
    }
    lines(blob)
        .windows(expected.len())
        .any(|window| window.iter().zip(expected).all(|(a, b)| *a == b.as_slice()))
}

fn lines(blob: &[u8]) -> Vec<&[u8]> {
    if blob.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<_> = blob.split(|byte| *byte == b'\n').collect();
    if blob.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

fn git_lines<const N: usize>(
    repository: &Path,
    args: [&str; N],
    operation: &str,
) -> Result<Vec<String>> {
    Ok(git_text(repository, args, operation)?
        .lines()
        .map(str::to_owned)
        .collect())
}

fn git_text<const N: usize>(repository: &Path, args: [&str; N], operation: &str) -> Result<String> {
    let output = git_output(repository, args)?;
    if !output.status.success() {
        return Err(git_failure(operation, &output));
    }
    String::from_utf8(output.stdout).context("Git emitted non-UTF-8 output")
}

fn git_ok<const N: usize>(repository: &Path, args: [&str; N], operation: &str) -> Result<()> {
    let output = git_output(repository, args)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(operation, &output))
    }
}

fn git_output<I, S>(repository: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .with_context(|| format!("could not run Git in {}", repository.display()))
}

fn command_ok(command: &mut Command, operation: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("could not {operation}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(operation, &output))
    }
}

fn git_failure(operation: &str, output: &Output) -> Error {
    let mut message = format!("could not {operation}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        let _ = write!(message, ": {stderr}");
    }
    anyhow!(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA1_HEX_LENGTH: usize = 40;

    fn test_git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn test_repository() -> (tempfile::TempDir, Repository) {
        let temporary = tempfile::tempdir().unwrap();
        test_git(temporary.path(), &["init", "--quiet", "-b", "main"]);
        test_git(temporary.path(), &["config", "user.name", "Test User"]);
        test_git(
            temporary.path(),
            &["config", "user.email", "test@example.com"],
        );
        let repository = Repository {
            path: temporary.path().to_owned(),
            default_branch: "main".to_owned(),
            default_ref: "HEAD".to_owned(),
        };
        (temporary, repository)
    }

    fn commit_file(repository: &Path, path: &str, contents: &str, title: &str) -> String {
        fs::write(repository.join(path), contents).unwrap();
        test_git(repository, &["add", "--", path]);
        test_git(repository, &["commit", "--quiet", "-m", title]);
        test_git(repository, &["rev-parse", "HEAD"])
    }

    #[test]
    fn parses_unwrapped_urls_and_trailing_punctuation() {
        let candidate = concat!(
            "https://github.com/o/r/blob/",
            "0123456789abcdef0123456789abcdef01234567",
            "/a%20b.rs#L2-L4)."
        );
        let url = parse_candidate(candidate).unwrap();
        assert_eq!(url.owner, "o");
        assert_eq!(url.repo, "r");
        assert_eq!(url.path, "a b.rs");
        assert_eq!((url.start, url.end), (2, 4));
    }

    #[test]
    fn requires_a_full_commit_hash() {
        assert!(parse_candidate("https://github.com/o/r/blob/0123456/a.rs#L1").is_none());
        assert!(parse_candidate("https://github.com/o/r/blob/main/a.rs#L1").is_none());
    }

    #[test]
    fn matches_exact_whole_contiguous_lines() {
        let expected = extract_lines(b"beta\n  gamma\n", 1, 2).unwrap();
        assert!(contains_lines(b"alpha\nbeta\n  gamma\ndelta\n", &expected));
        assert!(!contains_lines(
            b"alpha\nx beta\n  gamma\ndelta\n",
            &expected
        ));
        assert!(!contains_lines(b"alpha\nbeta\n gamma\ndelta\n", &expected));
    }

    #[test]
    fn line_ranges_are_one_based_and_inclusive() {
        assert_eq!(
            extract_lines(b"one\ntwo\nthree\n", 2, 3).unwrap(),
            vec![b"two".to_vec(), b"three".to_vec()]
        );
        assert!(extract_lines(b"one\n", 2, 2).is_err());
    }

    // Each filesystem mutation is confined to this test's temporary directory.
    #[cfg_attr(dylint_lib = "general", allow(non_thread_safe_call_in_test))]
    #[test]
    fn scan_includes_only_tracked_files_and_records_all_occurrences() {
        let temporary = tempfile::tempdir().unwrap();
        test_git(temporary.path(), &["init", "--quiet"]);
        fs::create_dir(temporary.path().join("target")).unwrap();
        let url = concat!(
            "https://github.com/o/r/blob/",
            "0123456789abcdef0123456789abcdef01234567",
            "/a.rs#L1"
        );
        fs::write(temporary.path().join("one.txt"), format!("x\n{url}\n")).unwrap();
        fs::write(temporary.path().join("target/two.txt"), url).unwrap();
        fs::write(temporary.path().join("untracked.txt"), url).unwrap();
        test_git(
            temporary.path(),
            &["add", "--", "one.txt", "target/two.txt"],
        );

        let mut scanned = Vec::new();
        let result = scan_with_counted(&temporary, |path, index, total| {
            scanned.push((path.to_owned(), index, total));
        })
        .unwrap();
        assert!(result.errors.is_empty());
        assert_eq!(result.urls.len(), 1);
        assert_eq!(result.urls[0].occurrences.len(), 2);
        assert_eq!(result.urls[0].occurrences[0].line, 2);
        assert!(scanned.contains(&(PathBuf::from("one.txt"), 1, 2)));
        assert!(scanned.contains(&(PathBuf::from("target/two.txt"), 2, 2)));
        assert!(!scanned.iter().any(|(path, _, _)| path == "untracked.txt"));
    }

    // Each filesystem mutation is confined to this test's temporary directory.
    #[cfg_attr(dylint_lib = "general", allow(non_thread_safe_call_in_test))]
    #[cfg(unix)]
    #[test]
    fn scan_does_not_follow_tracked_symlinks() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        test_git(temporary.path(), &["init", "--quiet"]);
        let url = concat!(
            "https://github.com/o/r/blob/",
            "0123456789abcdef0123456789abcdef01234567",
            "/a.rs#L1"
        );
        fs::write(temporary.path().join("untracked.txt"), url).unwrap();
        symlink("untracked.txt", temporary.path().join("tracked-link")).unwrap();
        test_git(temporary.path(), &["add", "--", "tracked-link"]);

        let mut scanned = Vec::new();
        let result = scan_with(&temporary, |path| scanned.push(path.to_owned())).unwrap();
        assert!(result.errors.is_empty());
        assert!(result.urls.is_empty());
        assert_eq!(scanned, [PathBuf::from("tracked-link")]);
    }

    // Each filesystem mutation is confined to this test's temporary directory.
    #[cfg_attr(dylint_lib = "general", allow(non_thread_safe_call_in_test))]
    #[test]
    fn local_check_classifies_without_a_remote() {
        let (temporary, repository) = test_repository();
        let start = commit_file(
            temporary.path(),
            "file.txt",
            "before\ntarget\nafter\n",
            "initial",
        );
        let found = FoundUrl {
            url: SourceUrl {
                text: format!("https://github.com/o/r/blob/{start}/file.txt#L2"),
                owner: "o".to_owned(),
                repo: "r".to_owned(),
                commit: start,
                path: "file.txt".to_owned(),
                start: 2,
                end: 2,
            },
            occurrences: Vec::new(),
        };

        assert!(check_local(&repository, &found).unwrap().is_none());
        commit_file(
            temporary.path(),
            "file.txt",
            "before\nchanged\nafter\n",
            "change target",
        );
        assert_eq!(
            check_local(&repository, &found).unwrap(),
            Some(vec![b"target".to_vec()])
        );
    }

    // Each filesystem mutation is confined to this test's temporary directory.
    #[cfg_attr(dylint_lib = "general", allow(non_thread_safe_call_in_test))]
    #[test]
    fn checking_runs_in_distinct_ordered_phases() {
        let (temporary, _) = test_repository();
        let start = commit_file(temporary.path(), "file.txt", "stable\ntarget\n", "initial");
        commit_file(
            temporary.path(),
            "file.txt",
            "stable\nchanged\n",
            "change target",
        );

        let cache = temporary.path().join("cache");
        let cached_repository = cache.join("o/r");
        fs::create_dir_all(cached_repository.parent().unwrap()).unwrap();
        let output = Command::new("git")
            .args(["clone", "--quiet"])
            .arg(temporary.path())
            .arg(&cached_repository)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            cached_repository.join(".git").is_dir(),
            "{} is not seeded; `check_urls_in_cache` would fall back to a real GitHub clone",
            cached_repository.display()
        );

        let found = |line| FoundUrl {
            url: SourceUrl {
                text: format!("https://github.com/o/r/blob/{start}/file.txt#L{line}"),
                owner: "o".to_owned(),
                repo: "r".to_owned(),
                commit: start.clone(),
                path: "file.txt".to_owned(),
                start: line,
                end: line,
            },
            occurrences: Vec::new(),
        };
        let mut events = Vec::new();
        let outcomes = check_urls_in_cache(&cache, vec![found(1), found(2)], |progress| {
            events.push(match progress {
                CheckProgress::Preparing {
                    repository,
                    index,
                    total,
                } => format!("preparing {repository} {index}/{total}"),
                CheckProgress::Checking { index, total, .. } => {
                    format!("checking {index}/{total}")
                }
                CheckProgress::InvestigationCount { total } => {
                    format!("investigation count {total}")
                }
                CheckProgress::Investigating { index, total, .. } => {
                    format!("investigating {index}/{total}")
                }
            });
        });

        assert_eq!(
            events,
            [
                "preparing o/r 1/1",
                "checking 1/2",
                "checking 2/2",
                "investigation count 1",
                "investigating 1/1",
            ]
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, CheckOutcome::Current { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, CheckOutcome::Stale(_)))
                .count(),
            1
        );
    }

    #[test]
    fn preparation_fetches_multiple_missing_commits_together() {
        let (temporary, _) = test_repository();
        let first = commit_file(temporary.path(), "file.txt", "one\n", "one");
        let second = commit_file(temporary.path(), "file.txt", "two\n", "two");
        commit_file(temporary.path(), "file.txt", "three\n", "three");

        let clone_directory = tempfile::tempdir().unwrap();
        let clone_path = clone_directory.path().join("clone");
        let remote = format!("file://{}", temporary.path().display());
        let output = Command::new("git")
            .args(["clone", "--quiet", "--depth", "1"])
            .arg(remote)
            .arg(&clone_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let repository = Repository {
            path: clone_path,
            default_branch: "main".to_owned(),
            default_ref: "refs/remotes/origin/main".to_owned(),
        };
        assert!(!repository.object_exists(&first).unwrap());
        assert!(!repository.object_exists(&second).unwrap());
        repository
            .fetch_commits([first.as_str(), second.as_str()])
            .unwrap();
        assert!(repository.object_exists(&first).unwrap());
        assert!(repository.object_exists(&second).unwrap());
    }

    #[test]
    fn history_search_follows_a_rename_before_finding_a_change() {
        let (temporary, repository) = test_repository();
        let start = commit_file(
            temporary.path(),
            "old.txt",
            "before\ntarget\n  exact\nafter\n",
            "initial",
        );
        test_git(temporary.path(), &["mv", "old.txt", "new.txt"]);
        test_git(
            temporary.path(),
            &["commit", "--quiet", "-m", "rename file"],
        );
        let rename = test_git(temporary.path(), &["rev-parse", "HEAD"]);
        let expected = vec![b"target".to_vec(), b"  exact".to_vec()];

        assert_eq!(
            repository
                .find_break(&start, &rename, "old.txt", &expected)
                .unwrap(),
            None
        );

        let changed = commit_file(
            temporary.path(),
            "new.txt",
            "before\ntarget\n exact\nafter\n",
            "change whitespace",
        );
        assert_eq!(
            repository
                .find_break(&start, &changed, "old.txt", &expected)
                .unwrap(),
            Some(changed)
        );
    }

    #[test]
    fn reports_side_branch_origin_and_default_branch_merge() {
        let (temporary, repository) = test_repository();
        let start = commit_file(
            temporary.path(),
            "file.txt",
            "keep\ntarget\nkeep\n",
            "initial",
        );
        test_git(temporary.path(), &["checkout", "--quiet", "-b", "feature"]);
        let original = commit_file(
            temporary.path(),
            "file.txt",
            "keep\nchanged\nkeep\n",
            "change target",
        );
        test_git(temporary.path(), &["checkout", "--quiet", "main"]);
        commit_file(
            temporary.path(),
            "unrelated.txt",
            "unrelated\n",
            "main work",
        );
        test_git(
            temporary.path(),
            &[
                "merge",
                "--quiet",
                "--no-ff",
                "feature",
                "-m",
                "merge feature",
            ],
        );
        let integration = test_git(temporary.path(), &["rev-parse", "HEAD"]);
        let expected = vec![b"target".to_vec()];

        let found_integration = repository
            .find_break(&start, &integration, "file.txt", &expected)
            .unwrap()
            .unwrap();
        assert_eq!(found_integration, integration);
        assert_eq!(
            repository
                .find_original_change(&found_integration, "file.txt", &expected, 0)
                .unwrap(),
            original
        );
    }

    #[test]
    fn stale_reports_offer_colored_and_plain_rendering() {
        let report = StaleReport {
            found: FoundUrl {
                url: SourceUrl {
                    text: concat!(
                        "https://github.com/o/r/blob/",
                        "0000000000000000000000000000000000000000",
                        "/f#L1"
                    )
                    .to_owned(),
                    owner: "o".to_owned(),
                    repo: "r".to_owned(),
                    commit: "0".repeat(SHA1_HEX_LENGTH),
                    path: "f".to_owned(),
                    start: 1,
                    end: 1,
                },
                occurrences: vec![Occurrence {
                    file: PathBuf::from("README.md"),
                    line: 3,
                }],
            },
            change_commit: CommitInfo {
                hash: "1".repeat(SHA1_HEX_LENGTH),
                date: "2026-01-01T00:00:00Z".to_owned(),
                title: "change".to_owned(),
            },
            merge_commit: CommitInfo {
                hash: "2".repeat(SHA1_HEX_LENGTH),
                date: "2026-01-02T00:00:00Z".to_owned(),
                title: "merge".to_owned(),
            },
        };

        let plain = report.to_string();
        assert!(!plain.contains("\x1b["));
        assert!(plain.contains("Stale URL:"));
        assert!(plain.contains("Merge commit:"));

        let colored = report.colored().to_string();
        assert!(colored.contains("\x1b[1;31mStale URL:\x1b[0m"));
        assert!(colored.contains("\x1b[1;33mChange commit:\x1b[0m"));
        assert!(colored.contains("\x1b[1;35mMerge commit:\x1b[0m"));
        assert!(colored.contains("README.md:3"));
        assert!(colored.contains(&format!("\x1b[0m{}", "1".repeat(SHA1_HEX_LENGTH))));
        assert!(!colored.contains(&format!("\x1b[36m{}", "1".repeat(SHA1_HEX_LENGTH))));
    }
}
