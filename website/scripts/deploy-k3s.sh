#!/bin/bash
set -euo pipefail

#===============================================================================
# os.beecoders.com - K3S Deployment Script (Podman)
#===============================================================================
# Packs the static site (website/: index.html, style.css, site.js, img/) into
# an nginx image, pushes it and deploys it to K3S on the VPS. There is no build
# step: what is in website/ is what gets served.
#
# Architecture (namespace: beecodersos-web):
#   • beecodersos-web  nginx (unprivileged, port 8080, deploy/nginx.conf).
#                      NodePort 32120 — Cloudflare Tunnel forwards
#                      https://os.beecoders.com here over HTTP.
#
# Registry:  beecodersregistry.azurecr.io  (reuses the existing acr-secret
#            ImagePullSecret — copied into this namespace on first deploy).
#
# Usage:
#   website/scripts/deploy-k3s.sh [all|check|build|push|deploy|status|logs|run|tunnel]
#
#   all      check → build → push → deploy → status (default)
#   check    the site's files are there and every local link/image resolves
#   build    build the image locally with podman
#   push     podman login + push the image
#   deploy   apply the manifests on the VPS and wait for the rollout
#   status   kubectl get on the namespace
#   logs     tail the nginx logs
#   run      serve the image locally on http://localhost:8120 (a preview)
#   tunnel   print the Cloudflare Tunnel ingress rule
#
# Version: the OS version (OS_VERSION in the Makefile) + the git sha,
# -dirty-<time> when website/ has uncommitted changes; every image is also
# pushed as :latest. `push` and `deploy` reuse the version of the last `build`
# (override with APP_VERSION=...).
#
# Required tools locally: podman, ssh, git
#===============================================================================

HOME_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_DIR="$(cd "$HOME_DIR/.." && pwd)"
cd "$HOME_DIR"

COMMAND="${1:-all}"

REGISTRY="${REGISTRY:-beecodersregistry.azurecr.io}"
NAMESPACE="beecodersos-web"
APP="beecodersos-web"
IMAGE="$REGISTRY/$APP"
NODE_PORT="${NODE_PORT:-32120}"
PUBLIC_HOST="${PUBLIC_HOST:-os.beecoders.com}"

VPS_IP="${VPS_IP:-212.47.77.32}"
VPS_USER="${VPS_USER:-bart}"

K8S_DIR="$HOME_DIR/deploy/k3s"
VERSION_FILE="$HOME_DIR/.deploy-k3s.version"

app_version() {
  local base sha
  base="$(sed -n 's/^OS_VERSION *:= *//p' "$ROOT_DIR/Makefile" 2>/dev/null)"
  base="${base:-0.0.0}"
  sha="$(git -C "$ROOT_DIR" rev-parse --short HEAD 2>/dev/null || echo nogit)"
  if [[ -n "$(git -C "$ROOT_DIR" status --porcelain -- website 2>/dev/null)" ]]; then
    echo "$base-$sha-dirty-$(date +%Y%m%d%H%M%S)"
  else
    echo "$base-$sha"
  fi
}
if [[ -z "${APP_VERSION:-}" ]]; then
  if [[ "$COMMAND" =~ ^(push|deploy)$ && -s "$VERSION_FILE" ]]; then
    APP_VERSION="$(cat "$VERSION_FILE")"
  else
    APP_VERSION="$(app_version)"
  fi
fi

ssh_vps() {
  local cmd="$1"
  ssh -o StrictHostKeyChecking=accept-new "$VPS_USER@$VPS_IP" "bash -lc $(printf %q "$cmd")"
}

KUBECTL_VPS='$([ -r "$HOME/.kube/config" ] && echo "env KUBECONFIG=$HOME/.kube/config kubectl" || echo "sudo k3s kubectl")'
kubectl_vps() {
  ssh_vps "$KUBECTL_VPS $1"
}

check_build_deps() { command -v podman >/dev/null 2>&1 || { echo "podman not found"; exit 1; }; }
check_remote_deps() { command -v ssh >/dev/null 2>&1 || { echo "ssh not found"; exit 1; }; }

# -----------------------------------------------------------------------------
check_site() {
  echo "==> Checking the site"
  local f missing=0
  for f in index.html style.css site.js Dockerfile deploy/nginx.conf; do
    [[ -s "$HOME_DIR/$f" ]] || { echo "missing: $f"; missing=1; }
  done
  # Every local src/href in the page must exist (external links and #anchors are skipped).
  while read -r f; do
    [[ -e "$HOME_DIR/$f" ]] || { echo "index.html refers to a missing file: $f"; missing=1; }
  done < <(grep -oE '(src|href)="[^"#:]+"' "$HOME_DIR/index.html" | sed -E 's/^[a-z]+="//; s/"$//' | sort -u)
  # The Content-Security-Policy forbids inline styles and scripts.
  if grep -qE ' style="|<script>' "$HOME_DIR/index.html"; then
    echo "index.html has an inline style or script: the CSP in deploy/nginx.conf blocks it"; missing=1
  fi
  [[ $missing == 0 ]] || exit 1
  echo "    ok"
}

build_image() {
  check_site
  echo "==> Building $IMAGE:$APP_VERSION"
  podman build --pull=newer \
    -t "$IMAGE:latest" -t "$IMAGE:$APP_VERSION" \
    -f "$HOME_DIR/Dockerfile" "$HOME_DIR"
  echo "$APP_VERSION" > "$VERSION_FILE"
}

push_image() {
  echo "==> Logging into $REGISTRY"
  if [[ -n "${REGISTRY_USER:-}" && -n "${REGISTRY_PASSWORD:-}" ]]; then
    podman login -u "$REGISTRY_USER" -p "$REGISTRY_PASSWORD" "$REGISTRY"
  else
    podman login "$REGISTRY"
  fi
  echo "==> Pushing $IMAGE"
  podman push "$IMAGE:$APP_VERSION"
  podman push "$IMAGE:latest"
}

# Copy the existing acr-secret from a namespace that has one. Idempotent.
ensure_acr_secret() {
  if kubectl_vps "-n $NAMESPACE get secret acr-secret >/dev/null 2>&1"; then
    return 0
  fi
  local src
  for src in beegit beedocs beehosting aidbooks sqail fietsloire; do
    if kubectl_vps "-n $src get secret acr-secret >/dev/null 2>&1"; then
      echo "==> $NAMESPACE/acr-secret missing; copying from $src/acr-secret"
      ssh_vps "K=$KUBECTL_VPS; \$K -n $src get secret acr-secret -o yaml \
        | sed -e '/namespace:/d' -e '/resourceVersion:/d' -e '/uid:/d' -e '/creationTimestamp:/d' \
        | \$K -n $NAMESPACE apply -f -"
      return 0
    fi
  done
  echo "ERROR: no source acr-secret found; create it with"
  echo "  kubectl -n $NAMESPACE create secret docker-registry acr-secret --docker-server=$REGISTRY --docker-username=<user> --docker-password=<token>"
  exit 1
}

deploy_manifests() {
  echo "==> Deploying $APP $APP_VERSION to $VPS_USER@$VPS_IP (namespace: $NAMESPACE)"
  ssh_vps "$KUBECTL_VPS apply -f -" < "$K8S_DIR/namespace.yaml"
  ensure_acr_secret
  local tmp
  tmp="$(mktemp -d)"
  cp "$K8S_DIR"/*.yaml "$tmp/"
  sed -i "s|newTag: .*|newTag: \"$APP_VERSION\"|" "$tmp/kustomization.yaml"
  sed -i -E "s|nodePort: *[0-9]+|nodePort: $NODE_PORT|" "$tmp/service.yaml"
  tar -C "$tmp" -czf - . | ssh -o StrictHostKeyChecking=accept-new "$VPS_USER@$VPS_IP" \
    "bash -lc $(printf %q "set -e; d=\$(mktemp -d); trap 'rm -rf \$d' EXIT; tar -xzf - -C \$d; $KUBECTL_VPS apply -k \$d")"
  rm -rf "$tmp"
  kubectl_vps "-n $NAMESPACE rollout status deployment/$APP --timeout=180s"
  echo ""
  echo "Deployed $APP $APP_VERSION"
  echo "  • Origin:  http://$VPS_IP:$NODE_PORT/"
  echo "  • Public:  https://$PUBLIC_HOST/  (via Cloudflare Tunnel)"
  echo ""
  tunnel_info
}

tunnel_info() {
  cat <<EOT
  Cloudflare Tunnel ingress rule (cloudflared on the VPS), before the catch-all:
    - hostname: $PUBLIC_HOST
      service: http://localhost:$NODE_PORT
  and a CNAME $PUBLIC_HOST -> <tunnel-id>.cfargotunnel.com (cloudflared tunnel route dns <tunnel> $PUBLIC_HOST).

EOT
}

status() { kubectl_vps "-n $NAMESPACE get pods,svc,deploy"; }
logs() { kubectl_vps "-n $NAMESPACE logs deployment/$APP --tail=80" || true; }

run_local() {
  [[ -n "$(podman images -q "$IMAGE:latest" 2>/dev/null)" ]] || build_image
  echo "==> http://localhost:8120/  (Ctrl+C stops)"
  podman run --rm -p 8120:8080 "$IMAGE:latest"
}

main() {
  case "$COMMAND" in
    all)
      check_build_deps; check_remote_deps
      build_image; push_image; deploy_manifests; status ;;
    check)   check_site ;;
    build)   check_build_deps; build_image ;;
    push)    check_build_deps; push_image ;;
    deploy)  check_remote_deps; deploy_manifests ;;
    status)  check_remote_deps; status ;;
    logs)    check_remote_deps; logs ;;
    run)     check_build_deps; run_local ;;
    tunnel)  tunnel_info ;;
    *)
      echo "Unknown command: $COMMAND"
      echo "Usage: $0 [all|check|build|push|deploy|status|logs|run|tunnel]"
      exit 1 ;;
  esac
}

main "$@"
