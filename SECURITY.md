# Security

Report vulnerabilities privately to security@awakenworks.com; do not open public
issues for security reports.

Foundation crates carry no secrets and no credential handling; secret material
(signing keys, provider credentials) lives in KMS / the deployment secret store
in the layers above, never here.
