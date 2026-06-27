/// Macro that defines a typed ID newtype backed by `String`.
///
/// Provides the same ergonomics as `String` for maps and format strings:
/// `Display`, `Deref<Target=str>`, `Borrow<str>`, `From<String>`,
/// `From<&str>`, `Clone`, `Hash`, `Eq`, and transparent serde.
macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                $name(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                $name(s.to_string())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }
    };
}

id_type!(AgentId);
id_type!(TraceId);
id_type!(SpanId);
id_type!(SpanEventId);
id_type!(CommandId);
id_type!(RuleId);
id_type!(EvalId);

/// Unix epoch milliseconds.
pub type Timestamp = i64;
