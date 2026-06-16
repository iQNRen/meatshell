#!/bin/bash
# 发布脚本：更新版本号、提交、打标签、推送、更新 Homebrew
# 用法: ./release.sh [patch|minor|major]

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

# 推送
git push origin rusterm
git push origin "v$NEW"

echo ""
echo "已推送 v$NEW，GitHub Actions 将自动构建发布"
echo ""
echo "构建完成后，更新 Homebrew："
echo "  cd ~/Desktop/myCodes/myProject/homebrew-tap"
echo "  ./update-cask.sh $NEW"
echo "  git add -A && git commit -m 'rusterm v$NEW' && git push"
