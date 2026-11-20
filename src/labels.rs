//! Readable names for the `@` state tokens.
//!
//! A token's *identity* stays the raw string — `@mv_112_134` — because that is
//! what gets compared, hashed, counted and written to checkpoints. These labels
//! are for display only. Keeping them separate matters: object names are not
//! unique (Mini-Zork has six rooms called `Maze`), so a token built from names
//! would silently merge distinct moves into one goal.
//!
//! ```text
//! @mv_112_134     thief → Maze#134
//! @f_28_13_1      Behind House flag13=1
//! @pp_28_13_1     Behind House prop13=1
//! @store_13_5     var13=5
//! ```

use std::collections::{HashMap, HashSet};

/// Object number to name, plus the set of names that need disambiguating.
#[derive(Debug, Default, Clone)]
pub struct Labels {
    names: HashMap<u16, String>,
    dupes: HashSet<String>,
}

impl Labels {
    /// Build from the text of folly's object tree.
    ///
    /// folly keeps `Object`'s fields private and offers no accessors, so the
    /// only handle on the table is its `Display` impl (`├── Grating Room (154)`
    /// per line). Parsing it is ugly, but the alternative is a one-line change
    /// in a dependency this crate does not own; if the format ever moves, every
    /// lookup falls back to `obj154` rather than breaking.
    pub fn from_tree(rendered: &str) -> Labels {
        let mut names: HashMap<u16, String> = HashMap::new();

        for line in rendered.lines() {
            // Split on the *last* ` (` so names may contain parentheses.
            let (head, tail) = match line.rsplit_once(" (") {
                Some(parts) => parts,
                None => continue,
            };
            let digits = match tail.strip_suffix(')') {
                Some(digits) => digits,
                None => continue,
            };
            let number: u16 = match digits.parse() {
                Ok(number) => number,
                Err(_) => continue,
            };
            let name = head.trim_start_matches(['|', ' ', '├', '└', '─']).trim();
            // Object 0 is the detach sentinel; folly spells nameless objects
            // out and those read better as plain numbers.
            if number == 0 || name.is_empty() || name == "(No Name)" || name == "(Null Object)" {
                continue;
            }
            names.insert(number, name.to_string());
        }

        let mut seen: HashMap<&str, usize> = HashMap::new();
        for name in names.values() {
            *seen.entry(name.as_str()).or_insert(0) += 1;
        }
        let dupes: HashSet<String> = seen
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(name, _)| name.to_string())
            .collect();

        Labels { names, dupes }
    }

    /// True when the object table could not be read at all.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Name of an object, numbered only when the name is shared.
    pub fn object(&self, number: u16) -> String {
        match self.names.get(&number) {
            Some(name) if self.dupes.contains(name) => format!("{}#{}", name, number),
            Some(name) => name.clone(),
            None => format!("obj{}", number),
        }
    }

    /// Where a `Move` landed. Object 0 means "detached from the tree".
    fn dest(&self, parent: u16) -> String {
        if parent == 0 {
            "null".to_string()
        } else {
            self.object(parent)
        }
    }

    /// Readable form of one token. Anything that is not an `@` token (printed
    /// text, mostly) and anything that does not parse is returned unchanged.
    pub fn label(&self, token: &str) -> String {
        if self.names.is_empty() {
            return token.to_string();
        }
        let rest = match token.strip_prefix('@') {
            Some(rest) => rest,
            None => return token.to_string(),
        };

        let parts: Vec<&str> = rest.split('_').collect();
        let label = match parts.as_slice() {
            ["mv", object, parent] => match (number(object), number(parent)) {
                (Some(object), Some(parent)) => {
                    format!("{} → {}", self.object(object), self.dest(parent))
                }
                _ => return token.to_string(),
            },
            ["f", object, flag, set] => match (number(object), number(flag)) {
                (Some(object), Some(flag)) => {
                    format!("{} flag{}={}", self.object(object), flag, set)
                }
                _ => return token.to_string(),
            },
            ["pp", object, property, set] => match (number(object), number(property)) {
                (Some(object), Some(property)) => {
                    format!("{} prop{}={}", self.object(object), property, set)
                }
                _ => return token.to_string(),
            },
            ["store", variable, value] => match number(variable) {
                Some(variable) => format!("var{}={}", variable, value),
                None => return token.to_string(),
            },
            _ => return token.to_string(),
        };
        label
    }
}

fn number(text: &str) -> Option<u16> {
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::Labels;

    const TREE: &str = "(Null Object) (0)
├── (No Name) (27)
|   ├── Thief's Lair (170)
|   |   └── chalice (175)
|   ├── Maze (23)
|   ├── Maze (57)
|   └── Cyclops Room (56)
├── you (30)
└── Behind House (28)
";

    fn labels() -> Labels {
        Labels::from_tree(TREE)
    }

    #[test]
    fn reads_numbers_and_names_out_of_the_tree() {
        let labels = labels();
        assert_eq!(labels.object(30), "you");
        assert_eq!(labels.object(56), "Cyclops Room");
        assert_eq!(labels.object(170), "Thief's Lair");
        // Object 0 and the nameless container are skipped.
        assert_eq!(labels.object(0), "obj0");
        assert_eq!(labels.object(27), "obj27");
    }

    #[test]
    fn shared_names_keep_their_number() {
        let labels = labels();
        assert_eq!(labels.object(23), "Maze#23");
        assert_eq!(labels.object(57), "Maze#57", "each Maze stays distinct");
    }

    #[test]
    fn state_tokens_become_readable() {
        let labels = labels();
        assert_eq!(labels.label("@mv_112_134"), "obj112 → obj134");
        assert_eq!(labels.label("@mv_30_28"), "you → Behind House");
        assert_eq!(labels.label("@mv_30_0"), "you → null");
        assert_eq!(labels.label("@f_28_13_1"), "Behind House flag13=1");
        assert_eq!(labels.label("@pp_30_7_1"), "you prop7=1");
        assert_eq!(labels.label("@store_13_5"), "var13=5");
    }

    #[test]
    fn foreign_text_passes_through() {
        let labels = labels();
        assert_eq!(
            labels.label("The house is a beautiful white"),
            "The house is a beautiful white"
        );
        assert_eq!(labels.label("@mv_notanumber"), "@mv_notanumber");
        assert_eq!(labels.label("@weird"), "@weird");
        // A checkpoint from another story: numbers we never saw.
        assert_eq!(labels.label("@mv_900_901"), "obj900 → obj901");
    }

    #[test]
    fn an_unreadable_tree_leaves_tokens_alone() {
        let labels = Labels::from_tree("no object table here");
        assert!(labels.is_empty());
        assert_eq!(labels.label("@mv_30_28"), "@mv_30_28");
    }
}
