//! Custom item definitions: what the client is told about the item (its item
//! components, dragonfly `iteminternal.Components`) and the data the server's rules read.

use super::{Category, Creative};
use crate::inventory::MAX_STACK;
use crate::item::{self, Class, Food};
use crate::nbt::{Compound, Value};

/// The `use_animation` of eating (dragonfly: 1 eat, 2 drink).
const EAT_ANIMATION: i32 = 1;

/// A custom item: a `static` definition the server is given with
/// [`ServerBuilder::custom_item`](crate::ServerBuilder::custom_item). Its icon comes from a
/// resource pack (`item_texture.json`).
///
/// ```
/// use anchor::custom::{Category, Creative, Item};
///
/// static RUBY: Item = Item::new("example:ruby", "Ruby", "example_ruby")
///     .with_creative(Creative::new(Category::Items).with_group("itemGroup.name.gems"));
/// static SWORD: Item = Item::new("example:sword", "Ruby Sword", "example_sword")
///     .with_durability(500)
///     .with_attack_damage(8)
///     .hand_equipped();
/// ```
#[derive(Debug)]
pub struct Item {
    identifier: &'static str,
    display_name: &'static str,
    icon: &'static str,
    info: item::Info,
    creative: Option<Creative>,
    off_hand_allowed: bool,
    hand_equipped: bool,
    glint: bool,
    breaks_in_creative: bool,
}

impl Item {
    /// The classes of a durable item.
    const CLASSES_DURABLE: [Class; 2] = [Class::Durable, Class::Item];

    /// The classes of a food.
    const CLASSES_FOOD: [Class; 2] = [Class::Food, Class::Item];

    /// The classes of a plain item.
    const CLASSES_ITEM: [Class; 1] = [Class::Item];

    /// The longest display name, in bytes.
    const MAX_DISPLAY_NAME_BYTES: usize = 256;

    /// The longest icon name, in bytes.
    const MAX_ICON_BYTES: usize = 256;

    /// An item named `identifier` (`namespace:name`, not in `minecraft`) that players see as
    /// `display_name` with the icon `icon`: stacks of 64, no durability, not food, not in the
    /// creative inventory, breaking blocks in creative mode as any item does.
    pub const fn new(
        identifier: &'static str,
        display_name: &'static str,
        icon: &'static str,
    ) -> Self {
        Self {
            identifier,
            display_name,
            icon,
            info: item::Info::custom(MAX_STACK, 0, 1, None),
            creative: None,
            off_hand_allowed: false,
            hand_equipped: false,
            glint: false,
            breaks_in_creative: true,
        }
    }

    /// The same item with the enchantment glint.
    #[must_use]
    pub const fn glint(mut self) -> Self {
        self.glint = true;
        self
    }

    /// The same item held like a tool (upright in the hand).
    #[must_use]
    pub const fn hand_equipped(mut self) -> Self {
        self.hand_equipped = true;
        self
    }

    /// The same item, allowed in the off-hand slot.
    #[must_use]
    pub const fn off_hand_allowed(mut self) -> Self {
        self.off_hand_allowed = true;
        self
    }

    /// The same item dealing `damage` per hit (1 for other items).
    #[must_use]
    pub const fn with_attack_damage(mut self, damage: u8) -> Self {
        self.info = item::Info::custom(
            self.info.max_stack(),
            self.info.max_durability(),
            damage,
            self.info.food(),
        );
        self
    }

    /// The same item in the creative inventory.
    #[must_use]
    pub const fn with_creative(mut self, creative: Creative) -> Self {
        self.creative = Some(creative);
        self
    }

    /// The same item wearing out after `durability` uses; such an item stacks to 1, and 0 leaves
    /// it without durability but still stacking to 1. It cannot be food.
    #[must_use]
    pub const fn with_durability(mut self, durability: u16) -> Self {
        self.info = item::Info::custom(1, durability, self.info.attack_points(), self.info.food());
        self
    }

    /// The same item eaten for `food` points and `saturation` (PMMP `Food`) when the player is
    /// hungry; [`with_food_always`](Self::with_food_always) makes one eaten at any time. It
    /// cannot have durability.
    #[must_use]
    pub const fn with_food(mut self, food: u8, saturation: f64) -> Self {
        self.info = item::Info::custom(
            self.info.max_stack(),
            self.info.max_durability(),
            self.info.attack_points(),
            Some(Food::new(food, saturation, true)),
        );
        self
    }

    /// The same item eaten for `food` points and `saturation` even when not hungry (like a
    /// golden apple).
    #[must_use]
    pub const fn with_food_always(mut self, food: u8, saturation: f64) -> Self {
        self.info = item::Info::custom(
            self.info.max_stack(),
            self.info.max_durability(),
            self.info.attack_points(),
            Some(Food::new(food, saturation, false)),
        );
        self
    }

    /// The same item in stacks of at most `size` (1 to 64).
    #[must_use]
    pub const fn with_max_stack_size(mut self, size: u8) -> Self {
        self.info = item::Info::custom(
            size,
            self.info.max_durability(),
            self.info.attack_points(),
            self.info.food(),
        );
        self
    }

    /// The same item, with which a player in creative mode breaks no block (like a vanilla
    /// sword): the client does not try, and the server refuses the break.
    #[must_use]
    pub const fn without_creative_breaking(mut self) -> Self {
        self.breaks_in_creative = false;
        self
    }

    /// Whether a player in creative mode breaks blocks with it.
    pub(crate) fn breaks_in_creative(&self) -> bool {
        self.breaks_in_creative
    }

    /// Why the definition cannot be registered, if it cannot.
    pub(crate) fn check(&self) -> Option<&'static str> {
        if self.display_name.is_empty() || self.display_name.len() > Self::MAX_DISPLAY_NAME_BYTES {
            return Some("the display name is empty or longer than 256 bytes");
        }
        if self.icon.is_empty() || self.icon.len() > Self::MAX_ICON_BYTES {
            return Some("the icon name is empty or longer than 256 bytes");
        }
        let stack = self.info.max_stack();
        if stack == 0 || stack > MAX_STACK {
            return Some("a stack size outside 1 to 64");
        }
        if self.info.max_durability() > 0 && stack != 1 {
            return Some("an item with durability stacks to more than 1");
        }
        if let Some(food) = self.info.food() {
            if self.info.max_durability() > 0 {
                return Some("an item with durability is also food");
            }
            if !food.saturation_restore().is_finite() || food.saturation_restore() < 0.0 {
                return Some("a food saturation that is negative or not finite");
            }
        }
        None
    }

    /// The PMMP classes the rules see: a food, a durable item or a plain item.
    pub(crate) fn classes(&self) -> &'static [Class] {
        if self.info.food().is_some() {
            &Self::CLASSES_FOOD
        } else if self.info.max_durability() > 0 {
            &Self::CLASSES_DURABLE
        } else {
            &Self::CLASSES_ITEM
        }
    }

    /// The `ItemRegistry` data of the item (dragonfly `iteminternal.Components`): the item
    /// properties (icon, creative place, stack size and the flags), the display name, and the
    /// durability and food components.
    pub(crate) fn components(&self) -> Compound {
        let (category, group) = match self.creative {
            Some(c) => (c.category, c.group.unwrap_or_default()),
            None => (Category::Items, ""),
        };
        let icon = Compound::from([(
            "textures".to_owned(),
            Value::Compound(Compound::from([(
                "default".to_owned(),
                Value::String(self.icon.to_owned()),
            )])),
        )]);
        let mut properties = Compound::from([
            (
                "creative_category".to_owned(),
                Value::Int(i32::from(category.id())),
            ),
            ("creative_group".to_owned(), Value::String(group.to_owned())),
            (
                "max_stack_size".to_owned(),
                Value::Int(i32::from(self.info.max_stack())),
            ),
            ("minecraft:icon".to_owned(), Value::Compound(icon)),
        ]);
        let mut flag = |key: &str, on: bool| {
            if on {
                properties.insert(key.to_owned(), Value::Byte(1));
            }
        };
        flag("allow_off_hand", self.off_hand_allowed);
        flag("hand_equipped", self.hand_equipped);
        flag("foil", self.glint);
        // The client breaks blocks in creative mode with an item that leaves the property out.
        if !self.breaks_in_creative {
            properties.insert("can_destroy_in_creative".to_owned(), Value::Byte(0));
        }
        if self.info.attack_points() != 1 {
            properties.insert(
                "damage".to_owned(),
                Value::Int(i32::from(self.info.attack_points())),
            );
        }
        let mut components = Compound::from([(
            "minecraft:display_name".to_owned(),
            Value::Compound(Compound::from([(
                "value".to_owned(),
                Value::String(self.display_name.to_owned()),
            )])),
        )]);
        if self.info.max_durability() > 0 {
            components.insert(
                "minecraft:durability".to_owned(),
                Value::Compound(Compound::from([(
                    "max_durability".to_owned(),
                    Value::Int(i32::from(self.info.max_durability())),
                )])),
            );
        }
        if let Some(food) = self.info.food() {
            let ticks = i32::from(self.info.use_ticks());
            properties.insert("use_duration".to_owned(), Value::Int(ticks));
            properties.insert("use_animation".to_owned(), Value::Int(EAT_ANIMATION));
            components.insert(
                "minecraft:food".to_owned(),
                Value::Compound(Compound::from([(
                    "can_always_eat".to_owned(),
                    Value::Byte(u8::from(!food.requires_hunger())),
                )])),
            );
        }
        components.insert("item_properties".to_owned(), Value::Compound(properties));
        Compound::from([("components".to_owned(), Value::Compound(components))])
    }

    /// Where it shows in the creative inventory.
    pub(crate) fn creative_place(&self) -> Option<Creative> {
        self.creative
    }

    /// The name the client shows for it, such as `Ruby`.
    pub const fn display_name(&self) -> &'static str {
        self.display_name
    }

    /// The identifier, such as `example:ruby`.
    pub const fn identifier(&self) -> &'static str {
        self.identifier
    }

    /// The data row the rules read.
    pub(crate) fn info(&'static self) -> &'static item::Info {
        &self.info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusals_name_the_limits_they_enforce() {
        let long: &'static str = String::leak("x".repeat(Item::MAX_DISPLAY_NAME_BYTES + 1));
        assert_eq!(
            Item::new("a:b", long, "i").check(),
            Some(
                format!(
                    "the display name is empty or longer than {} bytes",
                    Item::MAX_DISPLAY_NAME_BYTES
                )
                .as_str()
            )
        );
        let long: &'static str = String::leak("x".repeat(Item::MAX_ICON_BYTES + 1));
        assert_eq!(
            Item::new("a:b", "n", long).check(),
            Some(
                format!(
                    "the icon name is empty or longer than {} bytes",
                    Item::MAX_ICON_BYTES
                )
                .as_str()
            )
        );
        assert_eq!(
            Item::new("a:b", "n", "i")
                .with_max_stack_size(MAX_STACK + 1)
                .check(),
            Some(format!("a stack size outside 1 to {MAX_STACK}").as_str())
        );
    }

    #[test]
    fn an_item_with_durability_and_food_is_refused_in_either_order() {
        let reason = Some("an item with durability is also food");
        assert_eq!(
            Item::new("a:b", "n", "i")
                .with_food(1, 1.0)
                .with_durability(5)
                .check(),
            reason
        );
        assert_eq!(
            Item::new("a:b", "n", "i")
                .with_durability(5)
                .with_food(1, 1.0)
                .check(),
            reason
        );
    }
}
