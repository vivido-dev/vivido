use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;

use log::LevelFilter;
use serde::Deserialize;
use toml::Value;

/// Update an existing configuration value from a partial TOML value.
pub trait SerdeReplace {
    fn replace(&mut self, value: Value) -> Result<(), Box<dyn Error>>;
}

macro_rules! impl_replace {
    ($($ty:ty),*$(,)*) => {
        $(
            impl SerdeReplace for $ty {
                fn replace(&mut self, value: Value) -> Result<(), Box<dyn Error>> {
                    replace_simple(self, value)
                }
            }
        )*
    };
}

#[rustfmt::skip]
impl_replace!(
    usize, u8, u16, u32, u64, u128,
    isize, i8, i16, i32, i64, i128,
    f32, f64,
    bool,
    char,
    String,
    PathBuf,
    LevelFilter,
);

fn replace_simple<'de, D>(data: &mut D, value: Value) -> Result<(), Box<dyn Error>>
where
    D: Deserialize<'de>,
{
    *data = D::deserialize(value)?;
    Ok(())
}

impl<'de, T: Deserialize<'de>> SerdeReplace for Vec<T> {
    fn replace(&mut self, value: Value) -> Result<(), Box<dyn Error>> {
        replace_simple(self, value)
    }
}

impl<'de, T: SerdeReplace + Deserialize<'de>> SerdeReplace for Option<T> {
    fn replace(&mut self, value: Value) -> Result<(), Box<dyn Error>> {
        match self {
            Some(inner) => inner.replace(value),
            None => replace_simple(self, value),
        }
    }
}

impl<'de, T: Deserialize<'de>> SerdeReplace for HashMap<String, T> {
    fn replace(&mut self, value: Value) -> Result<(), Box<dyn Error>> {
        let replacement = Self::deserialize(value)?;
        self.extend(replacement);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use vivido_config_derive::ConfigDeserialize;

    #[test]
    fn replace_option_merges_nested_fields() {
        #[derive(ConfigDeserialize, Default, PartialEq, Eq, Debug)]
        struct ReplaceOption {
            a: usize,
            b: usize,
        }

        let mut subject: Option<ReplaceOption> = None;
        crate::SerdeReplace::replace(&mut subject, toml::from_str("a=1").unwrap()).unwrap();
        crate::SerdeReplace::replace(&mut subject, toml::from_str("b=2").unwrap()).unwrap();

        assert_eq!(subject, Some(ReplaceOption { a: 1, b: 2 }));
    }
}
