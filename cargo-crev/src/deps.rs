use semver::Version;
use std::path::PathBuf;

use crev_data::*;
use crev_lib::*;

use crate::{opts::*, prelude::*, shared::*, term};
use cargo::core::PackageId;
use std::{
    collections::{HashMap, HashSet},
    ops::Add,
};

mod print_term;
pub mod scan;

#[derive(Copy, Clone, Debug)]
/// Progress-bar kind of thing, you know?
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

impl Progress {
    pub fn is_valid(self) -> bool {
        self.done <= self.total
    }

    pub fn is_complete(self) -> bool {
        self.done >= self.total
    }
}

#[derive(Copy, Clone, Debug)]
/// A count of something, plus the "total" number of that thing.
///
/// This is kind of context-dependent
pub struct CountWithTotal<T = u64> {
    pub count: T, // or "known" in case of crate owners
    pub total: T,
}

impl<T> Add<CountWithTotal<T>> for CountWithTotal<T>
where
    T: Add<T>,
{
    type Output = CountWithTotal<<T as Add>::Output>;

    fn add(self, other: CountWithTotal<T>) -> Self::Output {
        CountWithTotal {
            count: self.count + other.count,
            total: self.total + other.total,
        }
    }
}

/// A set of set of owners
#[derive(Clone, Debug)]
pub struct OwnerSetSet(HashMap<PackageId, HashSet<String>>);

impl OwnerSetSet {
    fn new(pkg_id: PackageId, set: impl IntoIterator<Item = String>) -> Self {
        let mut owner_set = HashMap::new();

        owner_set.insert(pkg_id, set.into_iter().collect());

        OwnerSetSet(owner_set)
    }

    pub fn to_total_owners(&self) -> usize {
        let all_owners: HashSet<_> = self.0.iter().flat_map(|(_pkg, set)| set).collect();

        all_owners.len()
    }

    pub fn to_total_distinct_groups(&self) -> usize {
        let mut count = 0;

        'outer: for (group_i, (_pkg, group)) in self.0.iter().enumerate() {
            for (other_group_i, (_pkg, other_group)) in self.0.iter().enumerate() {
                if group_i == other_group_i {
                    continue;
                }

                if group.iter().all(|member| other_group.contains(member)) {
                    // there is an `other_group` that is a super-set of this `group`
                    continue 'outer;
                }
            }
            // there was no other_group that would contain all members of this one
            count += 1;
        }

        count
    }
}

impl std::ops::Add<OwnerSetSet> for OwnerSetSet {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        let mut set = self.0.clone();
        for (k, v) in other.0 {
            set.insert(k, v);
        }

        OwnerSetSet(set)
    }
}

/// Crate statistics - details that can be accumulated
/// by recursively including dependencies
#[derive(Clone, Debug)]
pub struct AccumulativeCrateDetails {
    pub trust: VerificationStatus,
    pub trusted_issues: CountWithTotal,
    pub verified: bool,
    pub loc: Option<usize>,
    pub geiger_count: Option<u64>,
    pub has_custom_build: bool,
    pub owner_set: OwnerSetSet,
}

fn sum_options<T>(a: Option<T>, b: Option<T>) -> Option<T::Output>
where
    T: Add<T>,
{
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        _ => None,
    }
}

impl std::ops::Add<AccumulativeCrateDetails> for AccumulativeCrateDetails {
    type Output = Self;

    #[allow(clippy::suspicious_arithmetic_impl)]
    fn add(self, other: Self) -> Self {
        Self {
            trust: self.trust.min(other.trust),
            trusted_issues: self.trusted_issues + other.trusted_issues,
            verified: self.verified && other.verified,
            loc: sum_options(self.loc, other.loc),
            geiger_count: sum_options(self.geiger_count, other.geiger_count),
            has_custom_build: self.has_custom_build || other.has_custom_build,
            owner_set: self.owner_set + other.owner_set,
        }
    }
}

/// Crate statistics - details
#[derive(Clone, Debug)]
pub struct CrateDetails {
    pub digest: Digest,
    pub latest_trusted_version: Option<Version>,
    pub trusted_reviewers: HashSet<PubId>,
    pub version_reviews: CountWithTotal,
    pub version_downloads: Option<CountWithTotal>,
    pub known_owners: Option<CountWithTotal>,
    pub unclean_digest: bool,
    pub accumulative_own: AccumulativeCrateDetails,
    pub accumulative: AccumulativeCrateDetails,
}

/// Basic crate info of a crate we're scanning
#[derive(Clone, Debug)]
pub struct CrateInfo {
    pub id: cargo::core::PackageId, // contains the name, version
    pub root: PathBuf,
    pub has_custom_build: bool,
}

impl CrateInfo {
    pub fn from_pkg(pkg: &cargo::core::Package) -> Self {
        let id = pkg.package_id();
        let root = pkg.root().to_path_buf();
        let has_custom_build = pkg.has_custom_build();
        CrateInfo {
            id,
            root,
            has_custom_build,
        }
    }

    pub fn download_if_needed(&self, cargo_opts: CargoOpts) -> Result<()> {
        if !self.root.exists() {
            let repo = crate::Repo::auto_open_cwd(cargo_opts)?;
            let mut source = repo.load_source()?;
            source.download(self.id)?;
        }
        Ok(())
    }
}

impl PartialOrd for CrateInfo {
    fn partial_cmp(&self, other: &CrateInfo) -> Option<std::cmp::Ordering> {
        self.id.partial_cmp(&other.id)
    }
}

impl Ord for CrateInfo {
    fn cmp(&self, other: &CrateInfo) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialEq for CrateInfo {
    fn eq(&self, other: &CrateInfo) -> bool {
        self.id == other.id
    }
}

impl Eq for CrateInfo {}

/// A dependency, as returned by the computer. It may
///  contain (depending on success/slipping) the computed
///  dep.
pub struct CrateStats {
    pub info: CrateInfo,
    pub details: Result<Option<CrateDetails>>,
}

impl CrateStats {
    pub fn is_digest_unclean(&self) -> bool {
        self.details().map_or(false, |d| d.unclean_digest)
    }

    pub fn has_details(&self) -> bool {
        self.details().is_some()
    }

    pub fn has_custom_build(&self) -> Option<bool> {
        self.details
            .as_ref()
            .ok()
            .and_then(|d| d.as_ref())
            .map(|d| &d.accumulative)
            .map(|a| a.has_custom_build)
    }

    pub fn details(&self) -> Option<&CrateDetails> {
        if let Ok(Some(ref details)) = self.details {
            Some(details)
        } else {
            None
        }
    }
}

pub fn latest_trusted_version_string(
    base_version: &Version,
    latest_trusted_version: &Option<Version>,
) -> String {
    if let Some(latest_trusted_version) = latest_trusted_version {
        format!(
            "{}{}",
            if base_version < latest_trusted_version {
                "↑"
            } else if latest_trusted_version < base_version {
                "↓"
            } else {
                "="
            },
            if base_version == latest_trusted_version {
                "".into()
            } else {
                latest_trusted_version.to_string()
            },
        )
    } else {
        "".to_owned()
    }
}

pub fn crate_mvps(common: CrateVerifyCommon) -> Result<()> {
    let mut args = CrateVerify::default();
    args.common = common;

    let scanner = scan::Scanner::new(&args)?;
    let events = scanner.run();

    let mut mvps: HashMap<PubId, u64> = HashMap::new();

    for stats in events {
        for reviewer in &stats.details?.expect("some").trusted_reviewers {
            *mvps.entry(reviewer.to_owned()).or_default() += 1;
        }
    }

    let mut mvps: Vec<_> = mvps.into_iter().collect();

    mvps.sort_by(|a, b| a.1.cmp(&b.1).reverse());

    for (id, count) in &mvps {
        println!("{:>3} {} {}", count, id.id, id.url.url);
    }

    Ok(())
}

pub fn verify_deps(args: CrateVerify) -> Result<CommandExitStatus> {
    let mut term = term::Term::new();

    let scanner = scan::Scanner::new(&args)?;
    let events = scanner.run();

    // print header, only after `scanner` had a chance to download everything
    if term.stderr_is_tty && term.stdout_is_tty {
        self::print_term::print_header(&mut term, args.verbose);
    }

    let deps: Vec<_> = events
        .into_iter()
        .map(|stats| {
            print_term::print_dep(&stats, &mut term, args.verbose, args.recursive)?;
            Ok(stats)
        })
        .collect::<Result<_>>()?;

    let mut nb_unclean_digests = 0;
    let mut nb_unverified = 0;
    for dep in &deps {
        if dep.is_digest_unclean() {
            let details = dep.details().unwrap();
            if details.unclean_digest {
                nb_unclean_digests += 1;
            }
            if !details.accumulative.verified {
                nb_unverified += 1;
            }
        }
    }

    if nb_unclean_digests > 0 {
        println!(
            "{} unclean package{} detected. Use `cargo crev clean <crate>` to wipe the local source.",
            nb_unclean_digests,
            if nb_unclean_digests > 1 { "s" } else { "" },
        );
        for dep in deps {
            if dep.is_digest_unclean() {
                term.eprint(
                    format_args!(
                        "Unclean crate {} {}\n",
                        &dep.info.id.name(),
                        &dep.info.id.version()
                    ),
                    ::term::color::RED,
                )?;
            }
        }
    }

    Ok(if nb_unverified == 0 {
        CommandExitStatus::Success
    } else {
        CommandExitStatus::VerificationFailed
    })
}
