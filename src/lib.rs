pub mod codec;
pub mod config;
pub mod protocol;
pub mod transport;

#[cfg(unix)]
pub mod process;

pub mod api;
pub mod obs;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

#[cfg(doctest)]
#[doc = include_str!("../docs/howto-library-usage.md")]
pub struct HowtoLibraryUsageDoctests;

#[cfg(doctest)]
#[doc = include_str!("../docs/howto-downloads-and-dialogs.md")]
pub struct HowtoDownloadsAndDialogsDoctests;

#[cfg(doctest)]
#[doc = include_str!("../docs/tutorial-getting-started.md")]
pub struct TutorialGettingStartedDoctests;

#[cfg(doctest)]
#[doc = include_str!("../docs/reference-library-api.md")]
pub struct ReferenceLibraryApiDoctests;

#[cfg(all(test, unix))]
#[path = "../tests/support/script.rs"]
mod test_script;
