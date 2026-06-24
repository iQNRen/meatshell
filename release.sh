#!/bin/bash
# 发布脚本：更新版本号、提交、打标签、推送
# 用法: ./release.sh [patch|minor|major]
# 推送失败时自动回滚版本号

set -e

# 获取当前版本
CURRENT=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
echo "当前版本: v$CURRENT"

# 计算新版本
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"
case "${1:-patch}" in
  patch) PATCH=$((PATCH + 1)) ;;
  minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
  major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
  *) echo "用法: $0 [patch|minor|major]"; exit 1 ;;
esac
NEW="$MAJOR.$MINOR.$PATCH"
echo "新版本: v$NEW"

# 更新 Cargo.toml
sed -i '' "s/version = \"$CURRENT\"/version = \"$NEW\"/" Cargo.toml

# 提交并打标签
git add Cargo.toml
git commit -m "v$NEW"
git tag "v$NEW"

# 检测代理是否可用
PROXY=""
if curl -s --connect-timeout 3 -x socks5h://127.0.0.1:10808 https://github.com > /dev/null 2>&1; then
  PROXY="socks5h://127.0.0.1:10808"
  echo "检测到代理，使用代理推送"
else
  echo "未检测到代理，直连推送"
fi

# git push --force-with-lease origin rusterm
# 推送（失败时回滚）
if git -c "http.proxy=$PROXY" -c "http.version=HTTP/1.1" push origin rusterm && \
   git -c "http.proxy=$PROXY" -c "http.version=HTTP/1.1" push origin "v$NEW"; then
  echo ""
  echo "已推送 v$NEW，GitHub Actions 将自动构建发布"
  echo ""
  echo "构建完成后，更新 Homebrew："
  echo "  cd ~/Desktop/myCodes/myProject/homebrew-tap"
  echo "  ./update-cask.sh $NEW"
  echo "  git add -A && git commit -m 'rusterm v$NEW' && git push"
else
  echo ""
  echo "推送失败！回滚版本号..."
  git tag -d "v$NEW"
  git reset --soft HEAD~1
  sed -i '' "s/version = \"$NEW\"/version = \"$CURRENT\"/" Cargo.toml
  git add Cargo.toml
  git commit -m "回滚: v$NEW → v$CURRENT（推送失败）"
  echo "已回滚到 v$CURRENT，请检查网络后重试"
  exit 1
fi
