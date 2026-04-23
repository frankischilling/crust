pub mod alias;

pub use alias::{
    expand_command_aliases, find_alias, AliasExpansion, CommandAlias, ExpandAliasError,
    MAX_ALIAS_DEPTH,
};
