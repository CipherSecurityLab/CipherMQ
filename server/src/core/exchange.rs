use std::collections::HashMap;

// Original Exchange struct from old state.rs
#[derive(Debug, Clone)]
pub struct Exchange {
    pub name: String,
    pub bindings: HashMap<String, Vec<String>>, 
}

impl Exchange {
    pub fn new(name: String) -> Self {
        Exchange {
            name,
            bindings: HashMap::new(),
        }
    }
}
