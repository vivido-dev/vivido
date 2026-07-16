#!/usr/bin/env bash

for x in {0..8}; do
    for i in {30..37}; do
        for a in {40..47}; do
            printf '\033[%d;%d;%dm\\\033[0m ' "$x" "$i" "$a"
        done
        printf '\n'
    done
done
printf '\n'
