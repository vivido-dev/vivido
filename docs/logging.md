# Diagnostic logging

Each diagnostic line is JSON with `elapsed_ms`, `level`, `target`, `event`, `message`, and
`fields`. Named properties from the `log` key/value API survive encoding. Existing messages
without an explicit event name use `legacy_message`. CLI results and build-script directives
remain their existing output formats.

Allowed properties are `event`, `error_kind`, `fault_id`, `class`, `window_id`, `connection_id`,
`count`, `bytes`, and `elapsed_ms`. Numeric and Boolean values retain their JSON types. Unknown
properties are redacted by default. Encoding retains at most 32 properties and 64 characters
per property name. Strings and legacy messages pass through bounded redaction that masks paths,
URLs, capability-shaped values, and named credentials.

Call sites must use static descriptions and scalar classifications. Do not interpolate terminal
text, clipboard contents, configuration values, commands, arguments, activation tokens, or
capability material. The redactor is defense in depth: it cannot identify arbitrary unlabeled
secrets. Configuration failures now use stable event names without source paths or parser
excerpts. Window-event diagnostics report classifications rather than keystrokes or event
payloads; terminal title changes report the action without the title.

Synthetic-secret tests cover unknown fields, named credentials, paths, and hexadecimal capability
values. Resource Debug implementations omit user content and avoid acquiring application locks.
The existing event density is preserved; these changes do not add per-byte or per-frame telemetry
in the parser or media path. Plain-text log consumers must migrate to the JSON schema.
