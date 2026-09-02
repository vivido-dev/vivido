//! In-crate configuration implementation helpers.
//!
//! These declarative macros replace the former `vivido_config_derive` proc-macro crate. Keeping
//! the implementations here means publishing `vivido` does not require publishing a second
//! package while preserving the forgiving configuration semantics.

macro_rules! config_warning {
    ($field:ident, $kind:literal, $message:literal) => {
        log::warn!(
            target: crate::logging::LOG_TARGET_CONFIG,
            concat!(
                "Config warning: ",
                stringify!($field),
                " has been ",
                $kind,
                "; ",
                $message,
                "\nUpdate your configuration file to resolve it"
            )
        );
    };
}

macro_rules! config_deserialize_value {
    ($config:ident, $value:ident, $field:ident) => {
        match serde::Deserialize::deserialize($value.clone()) {
            Ok(value) => $config.$field = value,
            Err(err) => {
                log::error!(
                    target: crate::logging::LOG_TARGET_CONFIG,
                    "Config error: {}: {}",
                    stringify!($field),
                    err.to_string().trim(),
                );
            },
        }
    };
    ($config:ident, $value:ident, option $field:ident) => {
        if $value.as_str().is_some_and(|value| value.eq_ignore_ascii_case("none")) {
            $config.$field = None;
        } else {
            config_deserialize_value!($config, $value, $field);
        }
    };
}

macro_rules! config_deserialize_field {
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident) => {
        if $key == stringify!($field) {
            $handled = true;
            config_deserialize_value!($config, $value, $field);
        }
    };
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: option) => {
        if $key == stringify!($field) {
            $handled = true;
            config_deserialize_value!($config, $value, option $field);
        }
    };
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: alias($alias:literal)) => {
        if $key == stringify!($field) || $key == $alias {
            $handled = true;
            config_deserialize_value!($config, $value, $field);
        }
    };
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: removed($message:literal)) => {
        if $key == stringify!($field) {
            $handled = true;
            config_deserialize_value!($config, $value, $field);
            config_warning!($field, "removed", $message);
        }
    };
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: alias_removed($alias:literal, $message:literal)) => {
        if $key == stringify!($field) || $key == $alias {
            $handled = true;
            config_deserialize_value!($config, $value, $field);
            config_warning!($field, "removed", $message);
        }
    };
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: option_alias_removed($alias:literal, $message:literal)) => {
        if $key == stringify!($field) || $key == $alias {
            $handled = true;
            config_deserialize_value!($config, $value, option $field);
            config_warning!($field, "removed", $message);
        }
    };
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: deprecated($message:literal)) => {
        if $key == stringify!($field) {
            $handled = true;
            config_deserialize_value!($config, $value, $field);
            config_warning!($field, "deprecated", $message);
        }
    };
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: option_deprecated($message:literal)) => {
        if $key == stringify!($field) {
            $handled = true;
            config_deserialize_value!($config, $value, option $field);
            config_warning!($field, "deprecated", $message);
        }
    };
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: skip) => {};
    ($config:ident, $key:ident, $value:ident, $handled:ident; $field:ident: flatten) => {};
}

macro_rules! config_deserialize_flatten {
    ($config:ident, $unused:ident; $field:ident: flatten) => {
        let flattened = std::mem::take(&mut $unused);
        $config.$field = serde::Deserialize::deserialize(flattened).unwrap_or_default();
    };
    ($config:ident, $unused:ident; $field:ident $(: $kind:ident $(($($args:literal),*))?)?) => {};
}

macro_rules! config_replace_field {
    ($self:ident, $field_name:ident, $next_value:ident, $handled:ident; $field:ident: flatten) => {};
    ($self:ident, $field_name:ident, $next_value:ident, $handled:ident; $field:ident: alias($alias:literal)) => {
        if $field_name == stringify!($field) || $field_name == $alias {
            $handled = true;
            crate::SerdeReplace::replace(&mut $self.$field, $next_value.clone())?;
        }
    };
    ($self:ident, $field_name:ident, $next_value:ident, $handled:ident; $field:ident: alias_removed($alias:literal, $message:literal)) => {
        if $field_name == stringify!($field) || $field_name == $alias {
            $handled = true;
            crate::SerdeReplace::replace(&mut $self.$field, $next_value.clone())?;
        }
    };
    ($self:ident, $field_name:ident, $next_value:ident, $handled:ident; $field:ident: option_alias_removed($alias:literal, $message:literal)) => {
        if $field_name == stringify!($field) || $field_name == $alias {
            $handled = true;
            crate::SerdeReplace::replace(&mut $self.$field, $next_value.clone())?;
        }
    };
    ($self:ident, $field_name:ident, $next_value:ident, $handled:ident; $field:ident $(: $kind:ident $(($($args:literal),*))?)?) => {
        if $field_name == stringify!($field) {
            $handled = true;
            crate::SerdeReplace::replace(&mut $self.$field, $next_value.clone())?;
        }
    };
}

macro_rules! config_replace_flatten {
    ($self:ident, $value:ident, $handled:ident; $field:ident: flatten) => {
        if !$handled {
            $handled = true;
            crate::SerdeReplace::replace(&mut $self.$field, $value.clone())?;
        }
    };
    ($self:ident, $value:ident, $handled:ident; $field:ident $(: $kind:ident $(($($args:literal),*))?)?) => {};
}

macro_rules! impl_config_deserialize {
    (
        $name:ident {
            $(
                $(#[$meta:meta])*
                $field:ident $(: $kind:ident $(($($args:literal),*))?)?
            ),* $(,)?
        }
    ) => {
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct ConfigVisitor;

                impl<'de> serde::de::Visitor<'de> for ConfigVisitor {
                    type Value = $name;

                    fn expecting(
                        &self,
                        formatter: &mut std::fmt::Formatter<'_>,
                    ) -> std::fmt::Result {
                        formatter.write_str("a mapping")
                    }

                    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
                    where
                        M: serde::de::MapAccess<'de>,
                    {
                        let mut config = Self::Value::default();
                        let mut unused = toml::Table::new();

                        while let Some((key, value)) =
                            map.next_entry::<String, toml::Value>()?
                        {
                            let mut handled = false;
                            $(
                                $(#[$meta])*
                                config_deserialize_field!(
                                    config,
                                    key,
                                    value,
                                    handled;
                                    $field $(: $kind $(($($args),*))?)?
                                );
                            )*

                            if !handled {
                                unused.insert(key, value);
                            }
                        }

                        $(
                            $(#[$meta])*
                            config_deserialize_flatten!(
                                config,
                                unused;
                                $field $(: $kind $(($($args),*))?)?
                            );
                        )*

                        for key in unused.keys() {
                            log::warn!(
                                target: crate::logging::LOG_TARGET_CONFIG,
                                "Unused config key: {key}"
                            );
                        }

                        Ok(config)
                    }
                }

                deserializer.deserialize_map(ConfigVisitor)
            }
        }

        impl crate::SerdeReplace for $name {
            fn replace(
                &mut self,
                value: toml::Value,
            ) -> Result<(), Box<dyn std::error::Error>> {
                match value.as_table() {
                    Some(table) => {
                        for (field_name, next_value) in table {
                            let mut handled = false;
                            $(
                                $(#[$meta])*
                                config_replace_field!(
                                    self,
                                    field_name,
                                    next_value,
                                    handled;
                                    $field $(: $kind $(($($args),*))?)?
                                );
                            )*
                            $(
                                $(#[$meta])*
                                config_replace_flatten!(
                                    self,
                                    value,
                                    handled;
                                    $field $(: $kind $(($($args),*))?)?
                                );
                            )*

                            if !handled {
                                return Err(format!("Field \"{field_name}\" does not exist").into());
                            }
                        }
                    },
                    None => *self = serde::Deserialize::deserialize(value)?,
                }

                Ok(())
            }
        }
    };
}

macro_rules! impl_config_deserialize_generic {
    (
        $name:ident<$generic:ident> {
            $($field:ident),* $(,)?
        }
    ) => {
        impl<'de, $generic> serde::Deserialize<'de> for $name<$generic>
        where
            $generic: Default + serde::Deserialize<'de> + crate::SerdeReplace,
        {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct ConfigVisitor<T>(std::marker::PhantomData<T>);

                impl<'de, T> serde::de::Visitor<'de> for ConfigVisitor<T>
                where
                    T: Default + serde::Deserialize<'de> + crate::SerdeReplace,
                {
                    type Value = $name<T>;

                    fn expecting(
                        &self,
                        formatter: &mut std::fmt::Formatter<'_>,
                    ) -> std::fmt::Result {
                        formatter.write_str("a mapping")
                    }

                    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
                    where
                        M: serde::de::MapAccess<'de>,
                    {
                        let mut config = Self::Value::default();
                        let mut unused = toml::Table::new();

                        while let Some((key, value)) =
                            map.next_entry::<String, toml::Value>()?
                        {
                            let mut handled = false;
                            $(
                                config_deserialize_field!(
                                    config,
                                    key,
                                    value,
                                    handled;
                                    $field
                                );
                            )*

                            if !handled {
                                unused.insert(key, value);
                            }
                        }

                        for key in unused.keys() {
                            log::warn!(
                                target: crate::logging::LOG_TARGET_CONFIG,
                                "Unused config key: {key}"
                            );
                        }

                        Ok(config)
                    }
                }

                deserializer.deserialize_map(ConfigVisitor::<$generic>(std::marker::PhantomData))
            }
        }

        impl<$generic> crate::SerdeReplace for $name<$generic>
        where
            $generic: Default + serde::de::DeserializeOwned + crate::SerdeReplace,
        {
            fn replace(
                &mut self,
                value: toml::Value,
            ) -> Result<(), Box<dyn std::error::Error>> {
                match value.as_table() {
                    Some(table) => {
                        for (field_name, next_value) in table {
                            match field_name.as_str() {
                                $(
                                    stringify!($field) => crate::SerdeReplace::replace(
                                        &mut self.$field,
                                        next_value.clone(),
                                    )?,
                                )*
                                _ => {
                                    return Err(
                                        format!("Field \"{field_name}\" does not exist").into()
                                    );
                                },
                            }
                        }
                    },
                    None => *self = serde::Deserialize::deserialize(value)?,
                }

                Ok(())
            }
        }
    };
}

macro_rules! impl_config_deserialize_enum {
    ($name:ident { $($variant:ident),* $(,)? }) => {
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct ConfigVisitor;

                impl<'de> serde::de::Visitor<'de> for ConfigVisitor {
                    type Value = $name;

                    fn expecting(
                        &self,
                        formatter: &mut std::fmt::Formatter<'_>,
                    ) -> std::fmt::Result {
                        let values = [$(stringify!($variant)),*].join("`, `");
                        write!(formatter, "one of `{values}`")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        $(
                            if value.eq_ignore_ascii_case(stringify!($variant)) {
                                return Ok($name::$variant);
                            }
                        )*

                        let values = [$(stringify!($variant)),*].join("`, `");
                        Err(E::custom(format!(
                            "unknown variant `{value}`, expected one of `{values}`"
                        )))
                    }
                }

                deserializer.deserialize_str(ConfigVisitor)
            }
        }

        impl_serde_replace!($name);
    };
}

macro_rules! impl_serde_replace {
    ($name:ty) => {
        impl crate::SerdeReplace for $name {
            fn replace(&mut self, value: toml::Value) -> Result<(), Box<dyn std::error::Error>> {
                *self = serde::Deserialize::deserialize(value)?;
                Ok(())
            }
        }
    };
}
