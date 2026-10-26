# Builtin skills

This directory contains awman's builtin skills: reusable instructions and reference material that can be pulled into a user's skill library. The first skill, `create-custom-agent`, helps an agent write a schema-1 custom agent definition for awman.

Pull or refresh the builtin library with:

```sh
awman new skill --pull builtin
```

The library slug is `awman`. Once pulled, mount the whole library with `skill(awman)`, or mount just this skill with `skill(awman/create-custom-agent)`. A hand-authored skill already named `awman` takes precedence over the pulled library.
