#!/bin/bash

# List of URLs
urls=(
    "https://releases.ubuntu.com/23.04/ubuntu-23.04-desktop-amd64.iso"
    "https://releases.ubuntu.com/23.04/ubuntu-23.04-live-server-amd64.iso"
    "https://releases.ubuntu.com/22.04.3/ubuntu-22.04.3-desktop-amd64.iso"
    "https://releases.ubuntu.com/22.04.3/ubuntu-22.04.3-live-server-amd64.iso"
    "https://releases.ubuntu.com/20.04.6/ubuntu-20.04.6-desktop-amd64.iso"
)

# Verify each URL
for url in "${urls[@]}"; do
    echo "Checking URL: $url"
    if curl --head --silent --fail "$url" > /dev/null; then
        echo "URL is valid!"
    else
        echo "URL is invalid or not reachable."
    fi
    echo ""
done
