// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/derived_config.rs
//! Deterministic reconstruction of an authenticated lower-sink template.

/// Values must be obtained and revalidated by the caller's sink owner.
pub(in crate::customer) struct Binding<'a> {
    pub path: &'a str,
    pub identity: &'a str,
    pub block_bytes: u64,
}

pub(super) fn safe_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 481
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
        && path[1..]
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

/// Reconstruct bytes only; this does not authenticate a template or a sink.
pub(in crate::customer) fn reconstruct(
    template: &[u8],
    binding: &Binding<'_>,
    limit: usize,
) -> Option<Vec<u8>> {
    if template.len() > limit
        || !safe_path(binding.path)
        || !crate::customer::package::is_lowercase_sha256(binding.identity)
        || !(512..=1_048_576).contains(&binding.block_bytes)
    {
        return None;
    }
    let text = std::str::from_utf8(template).ok()?;
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return None;
    }
    let values = [
        ("NBD_LOWER_SINK", "", binding.path.to_owned()),
        ("NBD_LOWER_SINK_TYPE", "directory", "directory".to_owned()),
        (
            "NBD_LOWER_SINK_IDENTITY_SHA256",
            "",
            binding.identity.to_owned(),
        ),
        (
            "NBD_LOWER_SINK_FS_BLOCK_BYTES",
            "",
            binding.block_bytes.to_string(),
        ),
        ("NBD_LOWER_SINK_BINDING", "unbound", "bound".to_owned()),
    ];
    let mut seen = [false; 5];
    let mut output = String::new();
    for line in text.split_inclusive('\n') {
        let without_newline = line.strip_suffix('\n').unwrap_or(line);
        if let Some((key, value)) = without_newline.split_once('=') {
            if let Some((index, (_, expected, replacement))) = values
                .iter()
                .enumerate()
                .find(|(_, (name, _, _))| *name == key)
            {
                if seen[index] || value != *expected {
                    return None;
                }
                seen[index] = true;
                output.push_str(key);
                output.push('=');
                output.push_str(replacement);
                output.push('\n');
            } else {
                output.push_str(line);
            }
        } else {
            output.push_str(line);
        }
        if output.len() > limit {
            return None;
        }
    }
    seen.into_iter()
        .all(|found| found)
        .then(|| output.into_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const TEMPLATE: &str = "# preserved\nNBD_LOWER_SINK=\nNBD_LOWER_SINK_TYPE=directory\nNBD_LOWER_SINK_IDENTITY_SHA256=\nNBD_LOWER_SINK_FS_BLOCK_BYTES=\nNBD_LOWER_SINK_BINDING=unbound\nOTHER=value\n";

    #[test]
    fn reconstructs_only_the_expected_unique_assignments() {
        let identity = "a".repeat(64);
        let binding = Binding {
            path: "/mnt/test/sink",
            identity: &identity,
            block_bytes: 4096,
        };
        let result = reconstruct(TEMPLATE.as_bytes(), &binding, 4096).unwrap();
        let expected = format!(
            "# preserved\nNBD_LOWER_SINK=/mnt/test/sink\nNBD_LOWER_SINK_TYPE=directory\nNBD_LOWER_SINK_IDENTITY_SHA256={identity}\nNBD_LOWER_SINK_FS_BLOCK_BYTES=4096\nNBD_LOWER_SINK_BINDING=bound\nOTHER=value\n"
        );
        assert_eq!(result, expected.as_bytes());
        assert!(reconstruct(TEMPLATE.as_bytes(), &binding, 16).is_none());
        assert!(
            reconstruct(
                format!("{TEMPLATE}NBD_LOWER_SINK=\n").as_bytes(),
                &binding,
                4096
            )
            .is_none()
        );
        assert!(
            reconstruct(
                TEMPLATE.replace("NBD_LOWER_SINK=\n", "").as_bytes(),
                &binding,
                4096
            )
            .is_none()
        );
        assert!(
            reconstruct(
                TEMPLATE
                    .replace(
                        "NBD_LOWER_SINK_BINDING=unbound",
                        "NBD_LOWER_SINK_BINDING=bound"
                    )
                    .as_bytes(),
                &binding,
                4096
            )
            .is_none()
        );
    }

    #[test]
    fn refuses_unsafe_binding_values_and_noncanonical_text() {
        let identity = "b".repeat(64);
        let binding = Binding {
            path: "/mnt/sink\ninjected=1",
            identity: &identity,
            block_bytes: 4096,
        };
        assert!(reconstruct(TEMPLATE.as_bytes(), &binding, 4096).is_none());
        let binding = Binding {
            path: "/mnt/../sink",
            identity: &identity,
            block_bytes: 4096,
        };
        assert!(reconstruct(TEMPLATE.as_bytes(), &binding, 4096).is_none());
        let binding = Binding {
            path: "/mnt/sink",
            identity: "NOT-A-HASH",
            block_bytes: 4096,
        };
        assert!(reconstruct(TEMPLATE.as_bytes(), &binding, 4096).is_none());
        let binding = Binding {
            path: "/mnt/sink",
            identity: &identity,
            block_bytes: 0,
        };
        assert!(reconstruct(TEMPLATE.as_bytes(), &binding, 4096).is_none());
        let binding = Binding {
            path: "/mnt/sink",
            identity: &identity,
            block_bytes: 4096,
        };
        assert!(reconstruct(TEMPLATE.replace('\n', "\r\n").as_bytes(), &binding, 4096).is_none());
    }
}
