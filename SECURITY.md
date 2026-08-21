# Security

Report a suspected vulnerability privately to the Repro It maintainers. Do not open a public issue
that contains an exploit, credential, customer payload, private endpoint, or captured World data.

Do not include secrets in logs or test fixtures. Use synthetic values for tests.

The CLI treats debugger capabilities, Cloud sessions, replay-host credentials, source credentials,
and managed project tokens as credentials. It must not print them.
