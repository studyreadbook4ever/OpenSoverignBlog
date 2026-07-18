# RBAC

Optional policy implementation. Removing it never produces `allow all`; the
kernel falls back to public published reads and authenticated single-owner
mutations. Grants are scoped by site, resource, and action. AuthN identities
from any provider enter through the same policy port.
