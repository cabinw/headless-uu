#!/bin/sh
# Install only the packaged binary. No account, login, dependency or service changes.
# Usage: ./install.sh [--prefix /usr/local]
set -eu
prefix=/usr/local
case ${1:-} in
  '') ;;
  --prefix)
    [ "$#" = 2 ] || { printf '%s\n' '--prefix 需要一个绝对路径 / --prefix requires one absolute path.' >&2; exit 2; }
    prefix=$2 ;;
  -h|--help)
    printf '%s\n' '用法 / Usage: ./install.sh [--prefix /usr/local]' \
      '仅安装二进制；登录和 systemd 请单独配置。/ Only installs the binary; configure login and systemd separately.'
    exit 0 ;;
  *) printf '%s\n' '未知参数，请查看 --help。/ Unknown arguments; see --help.' >&2; exit 2 ;;
esac
case $prefix in /*) ;; *) printf '%s\n' '安装前缀必须是绝对路径。/ Prefix must be absolute.' >&2; exit 2 ;; esac
src=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
[ -f "$src/uuremote-server" ] || { printf '%s\n' '安装包缺少 uuremote-server。/ Missing packaged uuremote-server.' >&2; exit 1; }
mkdir -p "$prefix/bin"
staged=$(mktemp "$prefix/bin/.uuremote-server.XXXXXX")
trap 'rm -f -- "$staged"' EXIT HUP INT TERM
install -m 755 "$src/uuremote-server" "$staged"
mv -f -- "$staged" "$prefix/bin/uuremote-server"
printf '已安装 / Installed: %s\n' "$prefix/bin/uuremote-server"
printf '%s\n' '账号登录与可选 systemd 服务见 README。/ Read README for account login and the optional systemd service.'
