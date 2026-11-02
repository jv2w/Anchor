//! The kinds of trees (PMMP `TreeType`), for [`Context::grow_tree`](crate::Context::grow_tree):
//! the trees saplings, azaleas and fungi grow.

/// A kind of tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
// api-class: TreeType
pub enum TreeType {
    /// Oak.
    #[doc(alias = "TreeType::OAK")]
    Oak,
    /// Spruce.
    #[doc(alias = "TreeType::SPRUCE")]
    Spruce,
    /// Birch, now and then a tall one.
    #[doc(alias = "TreeType::BIRCH")]
    Birch,
    /// Jungle, as one sapling grows it.
    #[doc(alias = "TreeType::JUNGLE")]
    Jungle,
    /// Acacia.
    #[doc(alias = "TreeType::ACACIA")]
    Acacia,
    /// Dark oak: PMMP has no generator for it, so it never grows.
    #[doc(alias = "TreeType::DARK_OAK")]
    DarkOak,
    /// A huge crimson fungus, with weeping vines.
    #[doc(alias = "TreeType::CRIMSON")]
    Crimson,
    /// A huge warped fungus.
    #[doc(alias = "TreeType::WARPED")]
    Warped,
    /// Azalea: an oak trunk under azalea leaves.
    #[doc(alias = "TreeType::AZALEA")]
    Azalea,
}

impl TreeType {
    /// The kind's name as PMMP shows it, such as `Dark Oak`.
    #[doc(alias = "TreeType::getDisplayName")]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Oak => "Oak",
            Self::Spruce => "Spruce",
            Self::Birch => "Birch",
            Self::Jungle => "Jungle",
            Self::Acacia => "Acacia",
            Self::DarkOak => "Dark Oak",
            Self::Crimson => "Crimson",
            Self::Warped => "Warped",
            Self::Azalea => "Azalea",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // api: TreeType, TreeType::getDisplayName
    #[test]
    fn tree_types_have_pmmps_display_names() {
        assert_eq!(TreeType::Oak.display_name(), "Oak");
        assert_eq!(TreeType::DarkOak.display_name(), "Dark Oak");
        assert_eq!(TreeType::Azalea.display_name(), "Azalea");
    }
}
