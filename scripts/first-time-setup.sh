#!/usr/bin/env sh
set -eu

# First-time setup for nib
echo "nib First-Time Setup"
echo "===================="

if ! command -v nib >/dev/null 2>&1; then
    echo "nib command not found. Please install nib first."
    exit 1
fi

echo "Running auth wizard for LLM providers..."
nib auth || true

echo ""
echo "First-time setup complete."
echo "Next: cd to a project and run 'nib chat' or 'nib run \"your goal\"'"
