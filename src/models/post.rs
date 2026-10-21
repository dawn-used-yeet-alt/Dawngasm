//! Post model.
//!
//! The serialized field names are the on-disk format shared with the web and
//! desktop apps, so they must not change.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Post {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub selftext: String,
    #[serde(default)]
    pub score: i64,
    #[serde(default)]
    pub num_comments: i64,
    #[serde(default)]
    pub created_utc: i64,
}

impl Post {
    /// The stable identity used for the "now playing" highlight and for
    /// de-duplicating saved posts and history.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.name
    }
}
