//! Tokens are strings, interned to small ids so the per-turn sets are cheap.
//!
//! `search.js` leans on JS `Set` string interning for the same effect
//! (`turntoks.add(stat.token)` -- "string interning", says the comment).

use std::collections::HashMap;
use std::rc::Rc;

pub type TokenId = u32;

pub struct Interner {
    ids: HashMap<String, TokenId>,
    /// `Rc` so the goal queue can carry a name for stable tie-breaking
    /// without copying the text again.
    names: Vec<Rc<str>>,
}

impl Interner {
    pub fn new() -> Interner {
        Interner {
            ids: HashMap::new(),
            names: Vec::new(),
        }
    }

    /// Returns the id, and whether this call created it.
    pub fn intern(&mut self, text: &str) -> (TokenId, bool) {
        match self.ids.get(text) {
            Some(&id) => (id, false),
            None => {
                let id = self.names.len() as TokenId;
                let name: Rc<str> = Rc::from(text);
                self.names.push(name);
                self.ids.insert(text.to_string(), id);
                (id, true)
            }
        }
    }

    pub fn get(&self, text: &str) -> Option<TokenId> {
        self.ids.get(text).copied()
    }

    pub fn name(&self, id: TokenId) -> &str {
        &self.names[id as usize]
    }

    pub fn shared(&self, id: TokenId) -> Rc<str> {
        Rc::clone(&self.names[id as usize])
    }
}
