use crate::util::data_structures::{HashMap, HashSet};
use std::cell::{Cell, RefCell};
use std::fmt::{self, Debug, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::ops;
use crate::sources::IndexSummary;
use crate::sources::source::MaybePackage;
use crate::sources::source::QueryKind;
use crate::sources::source::Source;
use crate::util::GlobalContext;
use crate::util::VersionReqMatchMode;
use crate::util::errors::CargoResult;
use crate::util::important_paths::find_project_manifest_exact;
use crate::util::internal;
use crate::workspace::parser::read_manifest;
use crate::workspace::{Dependency, EitherManifest, Manifest, Package, PackageId, SourceId};
use anyhow::Context as _;
use cargo_util::paths;
use filetime::FileTime;
use gix::bstr::{BString, ByteVec};
use gix::dir::entry::Status;
use gix::index::entry::Stage;
use ignore::gitignore::GitignoreBuilder;
use tracing::{debug, info, trace, warn};
use walkdir::WalkDir;

/// A source that represents a package gathered at the root
/// path on the filesystem.
///
/// It also provides convenient methods like [`PathSource::list_files`] to
/// list all files in a package, given its ability to walk the filesystem.
pub struct PathSource<'gctx> {
    /// The unique identifier of this source.
    source_id: SourceId,
    /// The root path of this source.
    path: PathBuf,
    /// The package discovered in this source, if any.
    package: RefCell<Option<Option<Package>>>,
    gctx: &'gctx GlobalContext,
}

impl<'gctx> PathSource<'gctx> {
    /// Invoked with an absolute path to a directory that contains a `Cargo.toml`.
    ///
    /// This source will only return the package at precisely the `path`
    /// specified, and it will be an error if there's not a package at `path`.
    pub fn new(path: &Path, source_id: SourceId, gctx: &'gctx GlobalContext) -> Self {
        Self {
            source_id,
            path: path.to_path_buf(),
            package: RefCell::new(None),
            gctx,
        }
    }

    /// Preloads a package for this source. The source is assumed that it has
    /// yet loaded any other packages.
    pub fn preload_with(pkg: Package, gctx: &'gctx GlobalContext) -> Self {
        let source_id = pkg.package_id().source_id();
        let path = pkg.root().to_owned();
        Self {
            source_id,
            path,
            package: RefCell::new(Some(Some(pkg))),
            gctx,
        }
    }

    /// Returns the root package, or an error if it is missing or failed to load.
    pub fn root_package(&self) -> CargoResult<Package> {
        trace!("root_package; source={:?}", self);

        self.load()?;

        match &*self.package.borrow() {
            Some(Some(pkg)) => Ok(pkg.clone()),
            Some(None) | None => Err(anyhow::format_err!(
                "failed to read `{}`",
                self.path.join("Cargo.toml").display()
            )),
        }
    }

    /// List all files relevant to building this package inside this source.
    ///
    /// This function will use the appropriate methods to determine the
    /// set of files underneath this source's directory which are relevant for
    /// building `pkg`.
    ///
    /// The basic assumption of this method is that all files in the directory
    /// are relevant for building this package, but it also contains logic to
    /// use other methods like `.gitignore`, `package.include`, or
    /// `package.exclude` to filter the list of files.
    #[tracing::instrument(skip_all)]
    pub fn list_files(&self, pkg: &Package) -> CargoResult<Vec<PathEntry>> {
        list_files(pkg, self.gctx)
    }

    /// Gets the last modified file in a package.
    fn last_modified_file(&self, pkg: &Package) -> CargoResult<(FileTime, PathBuf)> {
        if self.package.borrow().is_none() {
            return Err(internal(format!(
                "BUG: source `{:?}` was not loaded",
                self.path
            )));
        }
        last_modified_file(&self.path, pkg, self.gctx)
    }

    /// Returns the root path of this source.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Discovers packages inside this source if it hasn't yet done.
    pub fn load(&self) -> CargoResult<()> {
        let mut package = self.package.borrow_mut();
        if package.is_none() {
            *package = Some(self.read_package()?);
        }

        Ok(())
    }

    /// Reads the manifest. Returning `Ok(None)` if missing allows the resolver
    /// to handle it as "not found" instead of an early IO error.
    fn read_package(&self) -> CargoResult<Option<Package>> {
        let path = self.path.join("Cargo.toml");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(ops::read_package(&path, self.source_id, self.gctx)?))
    }
}

impl<'gctx> Debug for PathSource<'gctx> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "the paths source")
    }
}

#[async_trait::async_trait(?Send)]
impl<'gctx> Source for PathSource<'gctx> {
    async fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> CargoResult<()> {
        self.load()?;
        if let Some(Some(p)) = &*self.package.borrow() {
            let s = p.summary();
            let matched = match kind {
                QueryKind::Exact | QueryKind::RejectedVersions => {
                    dep.matches(s, VersionReqMatchMode::Default)
                }
                QueryKind::AlternativeNames => true,
                QueryKind::Normalized => dep.matches(s, VersionReqMatchMode::Default),
            };
            if matched {
                f(IndexSummary::Candidate(s.clone()))
            }
        }
        Ok(())
    }

    fn supports_checksums(&self) -> bool {
        false
    }

    fn requires_precise(&self) -> bool {
        false
    }

    fn source_id(&self) -> SourceId {
        self.source_id
    }

    async fn download(&self, id: PackageId) -> CargoResult<MaybePackage> {
        trace!("getting packages; id={}", id);
        self.load()?;
        let pkg = self.package.borrow();
        let pkg = pkg
            .as_ref()
            .and_then(|p| p.as_ref())
            .filter(|pkg| pkg.package_id() == id);
        pkg.cloned()
            .map(MaybePackage::Ready)
            .ok_or_else(|| internal(format!("failed to find {} in path source", id)))
    }

    async fn finish_download(&self, _id: PackageId, _data: Vec<u8>) -> CargoResult<Package> {
        panic!("no download should have started")
    }

    fn fingerprint(&self, pkg: &Package) -> CargoResult<String> {
        let (max, max_path) = self.last_modified_file(pkg)?;
        // Note that we try to strip the prefix of this package to get a
        // relative path to ensure that the fingerprint remains consistent
        // across entire project directory renames.
        let max_path = max_path.strip_prefix(&self.path).unwrap_or(&max_path);
        Ok(format!("{} ({})", max, max_path.display()))
    }

    fn describe(&self) -> String {
        match self.source_id.url().to_file_path() {
            Ok(path) => path.display().to_string(),
            Err(_) => self.source_id.to_string(),
        }
    }

    fn invalidate_cache(&self) {
        // Path source has no local cache.
    }

    fn set_quiet(&mut self, _quiet: bool) {
        // Path source does not display status
    }
}

/// A source that represents one or multiple packages gathered from a given root
/// path on the filesystem.
pub struct RecursivePathSource<'gctx> {
    /// The unique identifier of this source.
    source_id: SourceId,
    /// The root path of this source.
    path: PathBuf,
    /// Whether this source has loaded all package information it may contain.
    loaded: Cell<bool>,
    /// Packages that this sources has discovered.
    ///
    /// Tracking all packages for a given ID to warn on-demand for unused packages
    packages: RefCell<HashMap<PackageId, Vec<Package>>>,
    /// Avoid redundant unused package warnings
    warned_duplicate: RefCell<HashSet<PackageId>>,
    gctx: &'gctx GlobalContext,
}

impl<'gctx> RecursivePathSource<'gctx> {
    /// Creates a new source which is walked recursively to discover packages.
    ///
    /// This is similar to the [`PathSource::new`] method except that instead
    /// of requiring a valid package to be present at `root` the folder is
    /// walked entirely to crawl for packages.
    ///
    /// Note that this should be used with care and likely shouldn't be chosen
    /// by default!
    pub fn new(root: &Path, source_id: SourceId, gctx: &'gctx GlobalContext) -> Self {
        Self {
            source_id,
            path: root.to_path_buf(),
            loaded: Cell::new(false),
            packages: Default::default(),
            warned_duplicate: Default::default(),
            gctx,
        }
    }

    /// Returns the packages discovered by this source. It may walk the
    /// filesystem if package information haven't yet loaded.
    pub fn read_packages(&self) -> CargoResult<Vec<Package>> {
        self.load()?;
        Ok(self
            .packages
            .borrow()
            .iter()
            .map(|(pkg_id, v)| {
                first_package(
                    *pkg_id,
                    v,
                    &mut self.warned_duplicate.borrow_mut(),
                    self.gctx,
                )
                .clone()
            })
            .collect())
    }

    /// List all files relevant to building this package inside this source.
    ///
    /// This function will use the appropriate methods to determine the
    /// set of files underneath this source's directory which are relevant for
    /// building `pkg`.
    ///
    /// The basic assumption of this method is that all files in the directory
    /// are relevant for building this package, but it also contains logic to
    /// use other methods like `.gitignore`, `package.include`, or
    /// `package.exclude` to filter the list of files.
    pub fn list_files(&self, pkg: &Package) -> CargoResult<Vec<PathEntry>> {
        list_files(pkg, self.gctx)
    }

    /// Gets the last modified file in a package.
    fn last_modified_file(&self, pkg: &Package) -> CargoResult<(FileTime, PathBuf)> {
        if !self.loaded.get() {
            return Err(internal(format!(
                "BUG: source `{:?}` was not loaded",
                self.path
            )));
        }
        last_modified_file(&self.path, pkg, self.gctx)
    }

    /// Returns the root path of this source.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Discovers packages inside this source if it hasn't yet done.
    pub fn load(&self) -> CargoResult<()> {
        if !self.loaded.get() {
            self.packages
                .replace(read_packages(&self.path, self.source_id, self.gctx)?);
            self.loaded.set(true);
        }

        Ok(())
    }
}

impl<'gctx> Debug for RecursivePathSource<'gctx> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "the paths source")
    }
}

#[async_trait::async_trait(?Send)]
impl<'gctx> Source for RecursivePathSource<'gctx> {
    async fn query(
        &self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> CargoResult<()> {
        self.load()?;
        for s in self
            .packages
            .borrow()
            .iter()
            .filter(|(pkg_id, _)| pkg_id.name() == dep.package_name())
            .map(|(pkg_id, pkgs)| {
                first_package(
                    *pkg_id,
                    pkgs,
                    &mut self.warned_duplicate.borrow_mut(),
                    self.gctx,
                )
            })
            .map(|p| p.summary())
        {
            let matched = match kind {
                QueryKind::Exact | QueryKind::RejectedVersions => {
                    dep.matches(s, VersionReqMatchMode::Default)
                }
                QueryKind::AlternativeNames => true,
                QueryKind::Normalized => dep.matches(s, VersionReqMatchMode::Default),
            };
            if matched {
                f(IndexSummary::Candidate(s.clone()))
            }
        }
        Ok(())
    }

    fn supports_checksums(&self) -> bool {
        false
    }

    fn requires_precise(&self) -> bool {
        false
    }

    fn source_id(&self) -> SourceId {
        self.source_id
    }

    async fn download(&self, id: PackageId) -> CargoResult<MaybePackage> {
        trace!("getting packages; id={}", id);
        self.load()?;
        let pkgs = self.packages.borrow();
        let pkg = pkgs.get(&id);
        pkg.map(|pkgs| {
            first_package(id, pkgs, &mut self.warned_duplicate.borrow_mut(), self.gctx).clone()
        })
        .map(MaybePackage::Ready)
        .ok_or_else(|| internal(format!("failed to find {} in path source", id)))
    }

    async fn finish_download(&self, _id: PackageId, _data: Vec<u8>) -> CargoResult<Package> {
        panic!("no download should have started")
    }

    fn fingerprint(&self, pkg: &Package) -> CargoResult<String> {
        let (max, max_path) = self.last_modified_file(pkg)?;
        // Note that we try to strip the prefix of this package to get a
        // relative path to ensure that the fingerprint remains consistent
        // across entire project directory renames.
        let max_path = max_path.strip_prefix(&self.path).unwrap_or(&max_path);
        Ok(format!("{} ({})", max, max_path.display()))
    }

    fn describe(&self) -> String {
        match self.source_id.url().to_file_path() {
            Ok(path) => path.display().to_string(),
            Err(_) => self.source_id.to_string(),
        }
    }

    fn invalidate_cache(&self) {
        // Path source has no local cache.
    }

    fn set_quiet(&mut self, _quiet: bool) {
        // Path source does not display status
    }
}

/// Type that abstracts over [`gix::dir::entry::Kind`] and [`fs::FileType`].
#[derive(Debug, Clone, Copy)]
enum FileType {
    File { maybe_symlink: bool },
    Dir,
    Symlink,
    Other,
}

impl From<fs::FileType> for FileType {
    fn from(value: fs::FileType) -> Self {
        if value.is_file() {
            FileType::File {
                maybe_symlink: false,
            }
        } else if value.is_dir() {
            FileType::Dir
        } else if value.is_symlink() {
            FileType::Symlink
        } else {
            FileType::Other
        }
    }
}

impl From<gix::dir::entry::Kind> for FileType {
    fn from(value: gix::dir::entry::Kind) -> Self {
        use gix::dir::entry::Kind;
        match value {
            Kind::Untrackable => FileType::Other,
            Kind::File => FileType::File {
                maybe_symlink: false,
            },
            Kind::Symlink => FileType::Symlink,
            Kind::Directory | Kind::Repository => FileType::Dir,
        }
    }
}

/// [`PathBuf`] with extra metadata.
#[derive(Clone, Debug)]
pub struct PathEntry {
    path: PathBuf,
    ty: FileType,
    /// Whether this path was visited when traversing a symlink directory.
    under_symlink_dir: bool,
}

impl PathEntry {
    pub fn into_path_buf(self) -> PathBuf {
        self.path
    }

    /// Similar to [`std::path::Path::is_file`]
    /// but doesn't follow the symbolic link nor make any system call
    pub fn is_file(&self) -> bool {
        matches!(self.ty, FileType::File { .. })
    }

    /// Similar to [`std::path::Path::is_dir`]
    /// but doesn't follow the symbolic link nor make any system call
    pub fn is_dir(&self) -> bool {
        matches!(self.ty, FileType::Dir)
    }

    /// Similar to [`std::path::Path::is_symlink`]
    /// but doesn't follow the symbolic link nor make any system call
    ///
    /// If the path is not a symlink but under a symlink parent directory,
    /// this will return false.
    /// See [`PathEntry::is_symlink_or_under_symlink`] for an alternative.
    pub fn is_symlink(&self) -> bool {
        matches!(self.ty, FileType::Symlink)
    }

    /// Whether a path is a symlink or a path under a symlink directory.
    ///
    /// Use [`PathEntry::is_symlink`] to get the exact file type of the path only.
    pub fn is_symlink_or_under_symlink(&self) -> bool {
        self.is_symlink() || self.under_symlink_dir
    }

    /// Whether this path might be a plain text symlink.
    ///
    /// Git may check out symlinks as plain text files that contain the link texts,
    /// when either `core.symlinks` is `false`, or on Windows.
    pub fn maybe_plain_text_symlink(&self) -> bool {
        matches!(
            self.ty,
            FileType::File {
                maybe_symlink: true
            }
        )
    }
}

impl std::ops::Deref for PathEntry {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path.as_path()
    }
}

impl AsRef<PathBuf> for PathEntry {
    fn as_ref(&self) -> &PathBuf {
        &self.path
    }
}

fn first_package<'p>(
    pkg_id: PackageId,
    pkgs: &'p Vec<Package>,
    warned_duplicate: &mut HashSet<PackageId>,
    gctx: &GlobalContext,
) -> &'p Package {
    if pkgs.len() != 1 && warned_duplicate.insert(pkg_id) {
        let ignored = pkgs[1..]
            .iter()
            // We can assume a package with publish = false isn't intended to be seen
            // by users so we can hide the warning about those since the user is unlikely
            // to care about those cases.
            .filter(|pkg| pkg.publish().is_none())
            .collect::<Vec<_>>();
        if !ignored.is_empty() {
            use std::fmt::Write as _;

            let plural = if ignored.len() == 1 { "" } else { "s" };
            let mut msg = String::new();
            let _ = writeln!(&mut msg, "skipping duplicate package{plural} `{pkg_id}`:");
            for ignored in ignored {
                let manifest_path = ignored.manifest_path().display();
                let _ = writeln!(&mut msg, "  {manifest_path}");
            }
            let manifest_path = pkgs[0].manifest_path().display();
            let _ = writeln!(&mut msg, "in favor of {manifest_path}");
            let _ = gctx.shell().warn(msg);
        }
    }
    &pkgs[0]
}

/// List all files relevant to building this package inside this source.
///
/// This function will use the appropriate methods to determine the
/// set of files underneath this source's directory which are relevant for
/// building `pkg`.
///
/// The basic assumption of this method is that all files in the directory
/// are relevant for building this package, but it also contains logic to
/// use other methods like `.gitignore`, `package.include`, or
/// `package.exclude` to filter the list of files.
pub fn list_files(pkg: &Package, gctx: &GlobalContext) -> CargoResult<Vec<PathEntry>> {
    FileListPackage::new(pkg)
        .list_files(&mut gctx.shell())
        .with_context(|| {
            format!(
                "failed to determine list of files in {}",
                pkg.root().display()
            )
        })
}

/// Owned manifest inputs for enumeration, not a snapshot of the file list.
/// Workers rediscover Git state and walk the current filesystem on every call.
struct FileListPackage {
    root: PathBuf,
    include: Vec<String>,
    exclude: Vec<String>,
}

impl FileListPackage {
    fn new(pkg: &Package) -> Self {
        Self {
            root: pkg.root().to_path_buf(),
            include: pkg.manifest().include().to_vec(),
            exclude: pkg.manifest().exclude().to_vec(),
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn list_files(&self, shell: &mut crate::Shell) -> CargoResult<Vec<PathEntry>> {
        _list_files(self, shell)
    }
}

/// See [`PathSource::list_files`].
fn _list_files(pkg: &FileListPackage, shell: &mut crate::Shell) -> CargoResult<Vec<PathEntry>> {
    let root = pkg.root();
    let no_include_option = pkg.include.is_empty();
    let git_repo = if no_include_option {
        discover_gix_repo(root)?
    } else {
        None
    };

    let mut exclude_builder = GitignoreBuilder::new(root);
    if no_include_option && git_repo.is_none() {
        // no include option and not git repo discovered (see rust-lang/cargo#7183).
        exclude_builder.add_line(None, ".*")?;
    }
    for rule in &pkg.exclude {
        exclude_builder.add_line(None, rule)?;
    }
    let ignore_exclude = exclude_builder.build()?;

    let mut include_builder = GitignoreBuilder::new(root);
    for rule in &pkg.include {
        include_builder.add_line(None, rule)?;
    }
    let ignore_include = include_builder.build()?;

    let ignore_should_package = |relative_path: &Path, is_dir: bool| {
        // "Include" and "exclude" options are mutually exclusive.
        if no_include_option {
            !ignore_exclude
                .matched_path_or_any_parents(relative_path, is_dir)
                .is_ignore()
        } else {
            if is_dir {
                // Generally, include directives don't list every
                // directory (nor should they!). Just skip all directory
                // checks, and only check files.
                return true;
            }
            ignore_include
                .matched_path_or_any_parents(relative_path, /* is_dir */ false)
                .is_ignore()
        }
    };

    let filter = |path: &Path, is_dir: bool| {
        let Ok(relative_path) = path.strip_prefix(root) else {
            return false;
        };

        let rel = relative_path.as_os_str();
        if rel == "Cargo.lock" || rel == "Cargo.toml" {
            return true;
        }

        ignore_should_package(relative_path, is_dir)
    };

    // Attempt Git-prepopulate only if no `include` (see rust-lang/cargo#4135).
    if no_include_option {
        if let Some(repo) = git_repo {
            return list_files_gix(pkg, &repo, &filter, shell);
        }
    }
    let mut ret = Vec::new();
    list_files_walk(pkg.root(), &mut ret, true, &filter, shell)?;
    Ok(ret)
}

/// Returns [`Some(gix::Repository)`](gix::Repository) if the discovered repository
/// (searched upwards from `root`) contains a tracked `<root>/Cargo.toml`.
/// Otherwise, the caller should fall back on full file list.
fn discover_gix_repo(root: &Path) -> CargoResult<Option<gix::Repository>> {
    let mut repo = match gix::ThreadSafeRepository::discover(root) {
        Ok(repo) => repo.to_thread_local(),
        Err(e) => {
            tracing::debug!(
                "could not discover git repo at or above {}: {}",
                root.display(),
                e
            );
            return Ok(None);
        }
    };
    // gix 0.90 caches core.bare before merging config.worktree. Detect the
    // loaded worktree configuration even if it disables its own extension.
    let config = repo.config_snapshot();
    let worktree_config = config.boolean("extensions.worktreeConfig") == Some(true)
        || config
            .plumbing()
            .sections()
            .any(|section| section.meta().source == gix::config::Source::Worktree);
    let bare = config.boolean("core.bare");
    let configured_worktree = config
        .plumbing()
        .raw_value_with_section_filter("core.worktree", gix::config::section::is_trusted)
        .ok()
        .map(|(path, section)| (path, section.meta().source));
    // GIT_WORK_TREE takes precedence over core.bare. gix has already resolved
    // it, so do not override or reject the resulting workdir.
    let worktree_from_environment = configured_worktree
        .as_ref()
        .is_some_and(|(_, source)| *source == gix::config::Source::EnvOverride);
    let is_bare = worktree_config && bare == Some(true) && !worktree_from_environment;
    if repo.workdir().is_none() && worktree_config && bare == Some(false) {
        let workdir = match configured_worktree {
            Some((path, source)) => {
                anyhow::ensure!(!path.is_empty(), "core.worktree must not be empty");
                let path = gix::path::from_bstr(path)?.into_owned();
                // Match gix's normal open path: command/environment settings
                // are relative to cwd, repository settings to the Git directory.
                let path = match source {
                    gix::config::Source::Env
                    | gix::config::Source::Cli
                    | gix::config::Source::Api
                    | gix::config::Source::EnvOverride => repo.current_dir().join(path),
                    _ => {
                        let path = repo.path().join(path);
                        // Resolve symlinks before interpreting `..`, but keep
                        // logical paths when they name the same directory (for
                        // example, a repository below a symlinked /tmp).
                        let resolved = gix::path::realpath(&path)?;
                        let same_directory =
                            gix::path::normalize(path.as_path().into(), repo.current_dir())
                                .and_then(|logical| gix::path::realpath(logical).ok())
                                .is_some_and(|logical| logical == resolved);
                        if same_directory { path } else { resolved }
                    }
                };
                // Enumeration compares paths lexically, so .git/.. must not
                // survive in the restored workdir.
                gix::path::normalize(path.into(), repo.current_dir()).map(|p| p.into_owned())
            }
            None => {
                gix::discover::upwards(root)?
                    .0
                    .into_repository_and_work_tree_directories()
                    .1
            }
        };
        repo.set_workdir(workdir)
            .with_context(|| format!("failed to restore worktree for {}", repo.path().display()))?;
    }
    let index = repo
        .index_or_empty()
        .with_context(|| format!("failed to open git index at {}", repo.path().display()))?;
    let repo_root = repo.workdir().filter(|_| !is_bare).ok_or_else(|| {
        anyhow::format_err!(
            "did not expect repo at {} to be bare",
            repo.path().display()
        )
    })?;
    let repo_relative_path = match paths::strip_prefix_canonical(root, repo_root) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "cannot determine if path `{:?}` is in git repo `{:?}`: {:?}",
                root, repo_root, e
            );
            return Ok(None);
        }
    };
    let manifest_path = gix::path::join_bstr_unix_pathsep(
        gix::path::to_unix_separators_on_windows(gix::path::into_bstr(repo_relative_path)?),
        "Cargo.toml",
    );
    if index.entry_index_by_path(&manifest_path).is_ok() {
        return Ok(Some(repo));
    }
    // Package Cargo.toml is not in git, don't use git to guide our selection.
    Ok(None)
}

/// Lists files relevant to building this package inside this source by
/// traversing the git working tree, while avoiding ignored files.
///
/// This looks into Git sub-repositories as well, resolving them to individual files.
/// Symlinks to directories will also be resolved, but walked as repositories if they
/// point to one to avoid picking up `.git` directories.
fn list_files_gix(
    pkg: &FileListPackage,
    repo: &gix::Repository,
    filter: &dyn Fn(&Path, bool) -> bool,
    shell: &mut crate::Shell,
) -> CargoResult<Vec<PathEntry>> {
    debug!("list_files_gix {}", pkg.root().display());
    let options = repo
        .dirwalk_options()?
        .emit_untracked(gix::dir::walk::EmissionMode::Matching)
        .emit_ignored(None)
        .emit_tracked(true)
        .recurse_repositories(false)
        .symlinks_to_directories_are_ignored_like_directories(true)
        .emit_empty_directories(false);
    let index = repo.index_or_empty()?;
    let root = repo
        .workdir()
        .ok_or_else(|| anyhow::format_err!("can't list files on a bare repository"))?;
    assert!(
        root.is_absolute(),
        "BUG: paths used internally are absolute, and the repo inherits that"
    );

    let pkg_path = pkg.root();
    let repo_relative_pkg_path = pkg_path.strip_prefix(root).unwrap_or(Path::new(""));
    let target_prefix = gix::path::to_unix_separators_on_windows(gix::path::into_bstr(
        repo_relative_pkg_path.join("target/"),
    )?);
    let package_prefix =
        gix::path::to_unix_separators_on_windows(gix::path::into_bstr(repo_relative_pkg_path)?);

    let pathspec = {
        // Include the package root.
        let mut include = BString::from(":(top)");
        include.push_str(package_prefix.as_ref());

        // Exclude the target directory.
        let mut exclude = BString::from(":!(exclude,top)");
        exclude.push_str(target_prefix.as_ref());

        vec![include, exclude]
    };

    let mut files = Vec::<PathEntry>::new();
    let mut subpackages_found = Vec::new();
    for item in repo
        .dirwalk_iter(index.clone(), pathspec, Default::default(), options)?
        .filter(|res| {
            // Don't include Cargo.lock if it is untracked. Packaging will
            // generate a new one as needed.
            // Also don't include untrackable directory entries, like FIFOs.
            res.as_ref().map_or(true, |item| {
                item.entry.disk_kind != Some(gix::dir::entry::Kind::Untrackable)
                    && !(item.entry.status == Status::Untracked
                        && item.entry.rela_path == "Cargo.lock")
            })
        })
        .map(|res| {
            res.map(|item| {
                // Assumption: if a file tracked as a symlink in Git index, and
                // the actual file type on disk is file, then it might be a
                // plain text file symlink.
                // There are exceptions like the file has changed from a symlink
                // to a real text file, but hasn't been committed to Git index.
                // Exceptions may be rare so we're okay with this now.
                let maybe_plain_text_symlink = item.entry.index_kind
                    == Some(gix::dir::entry::Kind::Symlink)
                    && item.entry.disk_kind == Some(gix::dir::entry::Kind::File);
                (
                    item.entry.rela_path,
                    item.entry.disk_kind,
                    maybe_plain_text_symlink,
                )
            })
        })
        .chain(
            // Append entries that might be tracked in `<pkg_root>/target/`.
            index
                .prefixed_entries(target_prefix.as_ref())
                .unwrap_or_default()
                .iter()
                .filter(|entry| {
                    // probably not needed as conflicts prevent this to run, but let's be explicit.
                    entry.stage() == Stage::Unconflicted
                })
                .map(|entry| {
                    (
                        entry.path(&index).to_owned(),
                        // Do not trust what's recorded in the index, enforce checking the disk.
                        // This traversal is not part of a `status()`, and tracking things in `target/`
                        // is rare.
                        None,
                        false,
                    )
                })
                .map(Ok),
        )
    {
        let (rela_path, kind, maybe_plain_text_symlink) = item?;
        let file_path = root.join(gix::path::from_bstr(rela_path)?);
        if file_path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml") {
            // Keep track of all sub-packages found and also strip out all
            // matches we've found so far. Note, though, that if we find
            // our own `Cargo.toml`, we keep going.
            let path = file_path.parent().unwrap();
            if path != pkg_path {
                debug!("subpackage found: {}", path.display());
                files.retain(|p| !p.starts_with(path));
                subpackages_found.push(path.to_path_buf());
                continue;
            }
        }

        // If this file is part of any other sub-package we've found so far,
        // skip it.
        if subpackages_found.iter().any(|p| file_path.starts_with(p)) {
            continue;
        }

        let is_dir = kind.map_or(false, |kind| {
            if kind == gix::dir::entry::Kind::Symlink {
                // Symlinks must be checked to see if they point to a directory
                // we should traverse.
                file_path.is_dir()
            } else {
                kind.is_dir()
            }
        });
        if is_dir {
            // This could be a submodule, or a sub-repository. In any case, we prefer to walk
            // it with git-support to leverage ignored files and to avoid pulling in entire
            // .git repositories.
            match gix::open(&file_path) {
                Ok(sub_repo) => {
                    files.extend(list_files_gix(pkg, &sub_repo, filter, shell)?);
                }
                Err(_) => {
                    list_files_walk(&file_path, &mut files, false, filter, shell)?;
                }
            }
        } else if (filter)(&file_path, is_dir) {
            assert!(!is_dir);
            trace!("  found {}", file_path.display());
            let ty = match kind.map(Into::into) {
                Some(FileType::File { .. }) => FileType::File {
                    maybe_symlink: maybe_plain_text_symlink,
                },
                Some(ty) => ty,
                None => FileType::Other,
            };
            files.push(PathEntry {
                path: file_path,
                ty,
                // Git index doesn't include files from symlink directory,
                // symlink dirs are handled in `list_files_walk`.
                under_symlink_dir: false,
            });
        }
    }

    return Ok(files);
}

/// Lists files relevant to building this package inside this source by
/// walking the filesystem from the package root path.
///
/// This is a fallback for [`list_files_gix`] when the package
/// is not tracked under a Git repository.
fn list_files_walk(
    path: &Path,
    ret: &mut Vec<PathEntry>,
    is_root: bool,
    filter: &dyn Fn(&Path, bool) -> bool,
    shell: &mut crate::Shell,
) -> CargoResult<()> {
    let walkdir = WalkDir::new(path)
        .follow_links(true)
        // While this is the default, set it explicitly.
        // We need walkdir to visit the directory tree in depth-first order,
        // so we can ensure a path visited later be under a certain directory.
        .contents_first(false)
        .into_iter()
        .filter_entry(|entry| {
            let path = entry.path();
            let at_root = is_root && entry.depth() == 0;
            let is_dir = entry.file_type().is_dir();

            if !at_root && !filter(path, is_dir) {
                return false;
            }

            if !is_dir {
                return true;
            }

            // Don't recurse into any sub-packages that we have.
            if !at_root && path.join("Cargo.toml").exists() {
                return false;
            }

            // Skip root Cargo artifacts.
            if is_root
                && entry.depth() == 1
                && path.file_name().and_then(|s| s.to_str()) == Some("target")
            {
                return false;
            }

            true
        });

    let mut current_symlink_dir = None;
    for entry in walkdir {
        match entry {
            Ok(entry) => {
                let file_type = entry.file_type();

                match current_symlink_dir.as_ref() {
                    Some(dir) if entry.path().starts_with(dir) => {
                        // Still walk under the same parent symlink dir, so keep it
                    }
                    Some(_) | None => {
                        // Not under any parent symlink dir, update the current one.
                        current_symlink_dir = if file_type.is_dir() && entry.path_is_symlink() {
                            Some(entry.path().to_path_buf())
                        } else {
                            None
                        };
                    }
                }

                if file_type.is_file() || file_type.is_symlink() {
                    // We follow_links(true) here so check if entry was created from a symlink
                    let ty = if entry.path_is_symlink() {
                        FileType::Symlink
                    } else {
                        file_type.into()
                    };
                    ret.push(PathEntry {
                        path: entry.into_path(),
                        ty,
                        // This rely on contents_first(false), which walks in depth-first order
                        under_symlink_dir: current_symlink_dir.is_some(),
                    });
                }
            }
            Err(err) if err.loop_ancestor().is_some() => {
                shell.warn(err)?;
            }
            Err(err) => match err.path() {
                // If an error occurs with a path, filter it again.
                // If it is excluded, Just ignore it in this case.
                // See issue rust-lang/cargo#10917
                Some(path) if !filter(path, path.is_dir()) => {}
                // Otherwise, simply recover from it.
                // Don't worry about error skipping here, the callers would
                // still hit the IO error if they do access it thereafter.
                Some(path) => ret.push(PathEntry {
                    path: path.to_path_buf(),
                    ty: FileType::Other,
                    under_symlink_dir: false,
                }),
                None => return Err(err.into()),
            },
        }
    }

    Ok(())
}

/// Gets the last modified file in a package.
fn last_modified_file(
    path: &Path,
    pkg: &Package,
    gctx: &GlobalContext,
) -> CargoResult<(FileTime, PathBuf)> {
    last_modified_file_excluding(path, pkg, gctx, None)
}

/// Legacy build-script freshness ignores only this build's actual output roots.
/// Packaging keeps its existing file list; no directory-name heuristics apply.
pub(crate) fn fingerprint_excluding(
    pkg: &Package,
    gctx: &GlobalContext,
    output_roots: &[PathBuf],
) -> CargoResult<String> {
    PreparedPathFingerprint::new(pkg, output_roots).fingerprint(&mut gctx.shell())
}

/// Uses the already parsed manifest, without a worker-local GlobalContext.
pub(crate) struct PreparedPathFingerprint {
    pkg: FileListPackage,
    output_roots: Vec<PathBuf>,
}

impl PreparedPathFingerprint {
    pub fn new(pkg: &Package, output_roots: &[PathBuf]) -> Self {
        Self {
            pkg: FileListPackage::new(pkg),
            output_roots: output_roots.to_vec(),
        }
    }

    pub fn fingerprint(&self, shell: &mut crate::Shell) -> CargoResult<String> {
        let pkg = &self.pkg;
        let package_root = fs::canonicalize(pkg.root())
            .with_context(|| format!("failed to resolve package root {}", pkg.root().display()))?;
        let mut excluded = Vec::new();
        for root in &self.output_roots {
            match fs::canonicalize(root) {
                Ok(root) if root != package_root && root.starts_with(&package_root) => {
                    excluded.push(root);
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("failed to resolve output root {}", root.display())
                    });
                }
            }
        }
        let files = pkg.list_files(shell)?;
        let (max, path) = last_modified_from_files(pkg.root(), files, Some(&excluded))?;
        let relative = path.strip_prefix(pkg.root()).unwrap_or(&path);
        Ok(format!("{} ({})", max, relative.display()))
    }
}

fn last_modified_file_excluding(
    path: &Path,
    pkg: &Package,
    gctx: &GlobalContext,
    excluded: Option<&[PathBuf]>,
) -> CargoResult<(FileTime, PathBuf)> {
    let files = list_files(pkg, gctx).with_context(|| {
        format!(
            "failed to determine the most recently modified file in {}",
            pkg.root().display()
        )
    })?;
    last_modified_from_files(path, files, excluded)
}

fn last_modified_from_files(
    path: &Path,
    files: Vec<PathEntry>,
    excluded: Option<&[PathBuf]>,
) -> CargoResult<(FileTime, PathBuf)> {
    let mut max = FileTime::zero();
    let mut max_path = PathBuf::new();
    for file in files {
        if let Some(excluded) = excluded {
            match fs::canonicalize(&*file) {
                Ok(real) if excluded.iter().any(|root| real.starts_with(root)) => continue,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("failed to resolve source file {}", file.display())
                    });
                }
            }
        }
        // Preserve Source's historical best-effort stat behavior. The shared
        // planning/execution API instead distinguishes absent files from IO
        // errors, so an unreadable source cannot silently become reusable.
        let mtime = match paths::mtime(&file) {
            Ok(mtime) => mtime,
            Err(_) if excluded.is_none() => FileTime::zero(),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to stat source file {}", file.display()));
            }
        };
        // Break equal-mtime ties by path so filesystem iteration order does not
        // affect which file is recorded in the fingerprint.
        if mtime > max || (mtime == max && file.path.as_path() > max_path.as_path()) {
            max = mtime;
            max_path = file.into_path_buf();
        }
    }
    trace!("last modified file {}: {}", path.display(), max);
    Ok((max, max_path))
}

fn read_packages(
    path: &Path,
    source_id: SourceId,
    gctx: &GlobalContext,
) -> CargoResult<HashMap<PackageId, Vec<Package>>> {
    let mut all_packages = HashMap::default();
    let mut visited = HashSet::<PathBuf>::default();
    let mut errors = Vec::<anyhow::Error>::new();

    trace!(
        "looking for root package: {}, source_id={}",
        path.display(),
        source_id
    );

    walk(path, &mut |dir| {
        trace!("looking for child package: {}", dir.display());

        // Don't recurse into hidden/dot directories unless we're at the toplevel
        if dir != path {
            let name = dir.file_name().and_then(|s| s.to_str());
            if name.map(|s| s.starts_with('.')) == Some(true) {
                return Ok(false);
            }

            // Don't automatically discover packages across git submodules
            if dir.join(".git").exists() {
                return Ok(false);
            }
        }

        // Don't ever look at target directories
        if dir.file_name().and_then(|s| s.to_str()) == Some("target")
            && has_manifest(dir.parent().unwrap())
        {
            return Ok(false);
        }

        if has_manifest(dir) {
            read_nested_packages(
                dir,
                &mut all_packages,
                source_id,
                gctx,
                &mut visited,
                &mut errors,
            )?;
        }
        Ok(true)
    })?;

    if all_packages.is_empty() {
        match errors.pop() {
            Some(err) => Err(err),
            None => {
                if find_project_manifest_exact(path, "cargo.toml").is_ok() {
                    Err(anyhow::format_err!(
                        "could not find `Cargo.toml` in `{}`
help: found `cargo.toml`, consider renaming it to `Cargo.toml`",
                        path.display()
                    ))
                } else {
                    Err(anyhow::format_err!(
                        "could not find `Cargo.toml` in `{}`",
                        path.display()
                    ))
                }
            }
        }
    } else {
        Ok(all_packages)
    }
}

fn nested_paths(manifest: &Manifest) -> Vec<PathBuf> {
    let mut nested_paths = Vec::new();
    let normalized = manifest.normalized_toml();
    let dependencies = normalized
        .dependencies
        .iter()
        .chain(normalized.build_dependencies())
        .chain(normalized.dev_dependencies())
        .chain(
            normalized
                .target
                .as_ref()
                .into_iter()
                .flat_map(|t| t.values())
                .flat_map(|t| {
                    t.dependencies
                        .iter()
                        .chain(t.build_dependencies())
                        .chain(t.dev_dependencies())
                }),
        );
    for dep_table in dependencies {
        for dep in dep_table.values() {
            let cargo_util_schemas::manifest::InheritableDependency::Value(dep) = dep else {
                continue;
            };
            let cargo_util_schemas::manifest::TomlDependency::Detailed(dep) = dep else {
                continue;
            };
            let Some(path) = dep.path.as_ref() else {
                continue;
            };
            nested_paths.push(PathBuf::from(path.as_str()));
        }
    }
    nested_paths
}

fn walk(path: &Path, callback: &mut dyn FnMut(&Path) -> CargoResult<bool>) -> CargoResult<()> {
    if !callback(path)? {
        trace!("not processing {}", path.display());
        return Ok(());
    }

    // Ignore any permission denied errors because temporary directories
    // can often have some weird permissions on them.
    let dirs = match fs::read_dir(path) {
        Ok(dirs) => dirs,
        Err(ref e) if e.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => {
            let cx = format!("failed to read directory `{}`", path.display());
            let e = anyhow::Error::from(e);
            return Err(e.context(cx));
        }
    };
    let mut dirs = dirs.collect::<Vec<_>>();
    dirs.sort_unstable_by_key(|d| d.as_ref().ok().map(|d| d.file_name()));
    for dir in dirs {
        let dir = dir?;
        if dir.file_type()?.is_dir() {
            walk(&dir.path(), callback)?;
        }
    }
    Ok(())
}

fn has_manifest(path: &Path) -> bool {
    find_project_manifest_exact(path, "Cargo.toml").is_ok()
}

fn read_nested_packages(
    path: &Path,
    all_packages: &mut HashMap<PackageId, Vec<Package>>,
    source_id: SourceId,
    gctx: &GlobalContext,
    visited: &mut HashSet<PathBuf>,
    errors: &mut Vec<anyhow::Error>,
) -> CargoResult<()> {
    if !visited.insert(path.to_path_buf()) {
        return Ok(());
    }

    let manifest_path = find_project_manifest_exact(path, "Cargo.toml")?;

    let manifest = match read_manifest(&manifest_path, source_id, gctx) {
        Err(err) => {
            // Ignore malformed manifests found on git repositories
            //
            // git source try to find and read all manifests from the repository
            // but since it's not possible to exclude folders from this search
            // it's safer to ignore malformed manifests to avoid
            //
            // TODO: Add a way to exclude folders?
            info!(
                "skipping malformed package found at `{}`",
                path.to_string_lossy()
            );
            errors.push(err.into());
            return Ok(());
        }
        Ok(tuple) => tuple,
    };

    let manifest = match manifest {
        EitherManifest::Real(manifest) => manifest,
        EitherManifest::Virtual(..) => return Ok(()),
    };
    let nested = nested_paths(&manifest);
    let pkg = Package::new(manifest, &manifest_path);

    let pkg_id = pkg.package_id();
    all_packages.entry(pkg_id).or_default().push(pkg);

    // Registry sources are not allowed to have `path=` dependencies because
    // they're all translated to actual registry dependencies.
    //
    // We normalize the path here ensure that we don't infinitely walk around
    // looking for crates. By normalizing we ensure that we visit this crate at
    // most once.
    //
    // TODO: filesystem/symlink implications?
    if !source_id.is_registry() {
        for p in nested.iter() {
            let path = paths::normalize_path(&path.join(p));
            let result =
                read_nested_packages(&path, all_packages, source_id, gctx, visited, errors);
            // Ignore broken manifests found on git repositories.
            //
            // A well formed manifest might still fail to load due to reasons
            // like referring to a "path" that requires an extra build step.
            //
            // See https://github.com/rust-lang/cargo/issues/6822.
            if let Err(err) = result {
                if source_id.is_git() {
                    info!(
                        "skipping nested package found at `{}`: {:?}",
                        path.display(),
                        &err,
                    );
                    errors.push(err);
                } else {
                    return Err(err);
                }
            }
        }
    }

    Ok(())
}
