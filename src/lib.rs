//! The library half of nekorisu: the signing format and the release manifest,
//! shared by the `nekorisu` client and the maintainers' `nekorisu-release`
//! tool. Keys are two-layered (root and signing keys).

pub mod release;
pub mod sign;
