#!/usr/bin/env bash

set -e

release-plz update 2>&1 | tee bump_version_output

# Suffix to append to the version. Passed as an argument to this script.
SUFFIX="$1"

# Ensure the suffix starts with a dash if it's provided and not empty
if [ -n "$SUFFIX" ] && [[ "$SUFFIX" != -* ]]; then
    SUFFIX="-$SUFFIX"
fi

crates_bumped=()
while IFS= read -r line; do
  name=$(echo "$line" | awk -F"\`" '{print $2}')
  version=$(echo "$line" | awk -F"-> " '{print $2}')
  crates_bumped+=("${name}-v${version}")
done < <(cat bump_version_output | grep "^\*")

len=${#crates_bumped[@]}
if [[ $len -eq 0 ]]; then
  echo "No changes detected. Exiting without bumping any versions."
  exit 0
fi

commit_message="chore(release): "
for crate in "${crates_bumped[@]}"; do
    local version=$(echo "$crate" | cut -d'v' -f2)
    local new_version="$version$SUFFIX"
    echo "-0--->>>> $crate new v is: $new_version"
  # commit_message="${commit_message}${crate}/"
done
# commit_message=${commit_message%/} # strip off trailing '/' character

# git add --all
# git commit -m "$commit_message"
# echo "Generated release commit: $commit_message"
