//! The pictures a painting shows (PMMP `PaintingMotive`): Bedrock's closed set with their sizes in
//! blocks, in PMMP's order (a placed painting picks among the largest that fit).

/// Declares [`PaintingMotive`] and its table from one list, so a name, a size and a variant are
/// written once.
macro_rules! motives {
    ($($variant:ident $name:literal $width:literal $height:literal,)*) => {
        /// A painting's picture (PMMP `PaintingMotive`): its name is what clients draw, its size
        /// how many blocks the painting covers. The set is closed: a client draws only the
        /// pictures its resources hold (PMMP's `registerMotive` adds names no client knows).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        // api-class: PaintingMotive
        pub enum PaintingMotive {
            $(
                #[doc = concat!("`", $name, "`, ", stringify!($width), " × ", stringify!($height),
                    " blocks.")]
                $variant,
            )*
        }

        impl PaintingMotive {
            /// The motive named `name` (PMMP `getMotiveByName`; names are case-sensitive, as
            /// saved).
            #[doc(alias = "PaintingMotive::getMotiveByName")]
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)*
                    _ => None,
                }
            }

            /// Every motive, in PMMP's order (PMMP `getAll`).
            #[doc(alias = "PaintingMotive::getAll")]
            pub fn all() -> &'static [Self] {
                &[$(Self::$variant,)*]
            }

            /// Its height in blocks, 1 to 4 (PMMP `getHeight`).
            #[doc(alias = "PaintingMotive::getHeight")]
            pub fn height(self) -> u8 {
                match self {
                    $(Self::$variant => $height,)*
                }
            }

            /// The name clients know it by and saves keep (PMMP `getName`), such as `Kebab`.
            #[doc(alias = "PaintingMotive::getName")]
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// Its width in blocks, 1 to 4 (PMMP `getWidth`).
            #[doc(alias = "PaintingMotive::getWidth")]
            pub fn width(self) -> u8 {
                match self {
                    $(Self::$variant => $width,)*
                }
            }
        }
    };
}

motives! {
    Alban "Alban" 1 1,
    Aztec "Aztec" 1 1,
    Aztec2 "Aztec2" 1 1,
    Bomb "Bomb" 1 1,
    Kebab "Kebab" 1 1,
    Meditative "meditative" 1 1,
    Plant "Plant" 1 1,
    Wasteland "Wasteland" 1 1,
    Graham "Graham" 1 2,
    PrairieRide "prairie_ride" 1 2,
    Wanderer "Wanderer" 1 2,
    Courbet "Courbet" 2 1,
    Creebet "Creebet" 2 1,
    Pool "Pool" 2 1,
    Sea "Sea" 2 1,
    Sunset "Sunset" 2 1,
    Bust "Bust" 2 2,
    Baroque "baroque" 2 2,
    Earth "Earth" 2 2,
    Fire "Fire" 2 2,
    Humble "humble" 2 2,
    Match "Match" 2 2,
    SkullAndRoses "SkullAndRoses" 2 2,
    Stage "Stage" 2 2,
    Void "Void" 2 2,
    Water "Water" 2 2,
    Wind "Wind" 2 2,
    Wither "Wither" 2 2,
    Bouquet "bouquet" 3 3,
    Cavebird "cavebird" 3 3,
    Cotan "cotan" 3 3,
    Endboss "endboss" 3 3,
    Fern "fern" 3 3,
    Owlemons "owlemons" 3 3,
    Sunflowers "sunflowers" 3 3,
    Tides "tides" 3 3,
    Backyard "backyard" 3 4,
    Pond "pond" 3 4,
    Changing "changing" 4 2,
    Fighters "Fighters" 4 2,
    Finding "finding" 4 2,
    Lowmist "lowmist" 4 2,
    Passage "passage" 4 2,
    DonkeyKong "DonkeyKong" 4 3,
    Skeleton "Skeleton" 4 3,
    BurningSkull "BurningSkull" 4 4,
    Orb "orb" 4 4,
    Pigscene "Pigscene" 4 4,
    Pointer "Pointer" 4 4,
    Unpacked "unpacked" 4 4,
}

#[cfg(test)]
mod tests {
    use super::*;

    // api: PaintingMotive, PaintingMotive::getAll, PaintingMotive::getName,
    // api: PaintingMotive::getWidth, PaintingMotive::getHeight, PaintingMotive::getMotiveByName
    #[test]
    fn motives_are_pmmps_fifty_with_their_sizes() {
        let all = PaintingMotive::all();
        assert_eq!(all.len(), 50);
        assert_eq!(all[0], PaintingMotive::Alban);
        assert_eq!(all[49], PaintingMotive::Unpacked);
        for &m in all {
            assert_eq!(PaintingMotive::from_name(m.name()), Some(m));
            assert!((1..=4).contains(&m.width()) && (1..=4).contains(&m.height()));
        }
        assert_eq!(PaintingMotive::from_name("kebab"), None, "names are exact");
        let backyard = PaintingMotive::Backyard;
        assert_eq!(
            (backyard.name(), backyard.width(), backyard.height()),
            ("backyard", 3, 4)
        );
        assert_eq!(PaintingMotive::Skeleton.width(), 4);
        assert_eq!(PaintingMotive::Skeleton.height(), 3);
    }
}
