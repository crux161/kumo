#!/bin/sh

echo "---- Round 1 ----"
git log --author="$(git config user.name)" --shortstat | awk '/files? changed/ {files+=$1; inserted+=$4; deleted+=$6; count++} END {printf "Commits: %d\nAvg Files/Commit: %.2f\nAvg Lines Added: %.2f\nAvg Lines Deleted: %.2f\n", count, files/count, inserted/count, deleted/count}'
echo "----  Done   ----"

echo "---- Round 2 ----"
git log --author="$(git config user.name)" --shortstat | awk '/files? changed/ { if ($4 < 1000) { files+=$1; inserted+=$4; deleted+=$6; count++ } } END { printf "Filtered Commits (under 1k lines): %d\nAvg Files/Commit: %.2f\nAvg Lines Added: %.2f\nAvg Lines Deleted: %.2f\n", count, files/count, inserted/count, deleted/count }'

echo "----  Done   ----"
