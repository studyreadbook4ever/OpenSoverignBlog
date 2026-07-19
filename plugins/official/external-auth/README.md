# External authentication bridge

Optional AuthN only. It maps a verified external subject into the kernel's
principal contract; it never decides authorization. Provider discovery,
callback verification, issuer/audience checks, PKCE, replay protection, and
secret rotation belong to the selected adapter.

When absent, the server uses the selected access-key module or stays read-only.
A reverse-proxy header adapter must define trusted proxy addresses and
must reject client-supplied identity headers at the public edge.
