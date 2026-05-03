# new-amux observed issues

### ISSUE-1

1.1: Calling `new spec` does not ask what kind of work item it should be, and therefore does not replace the type placeholder in the resulting file. Ensure it behaves just like old-amux.

1.2 Passing `--interview` to `new spec` does not ask for the work item's interview prompt. Ensure this works for all the `new *` commands

1.3 Passing `--interview` to `new spec` results in an agent container with no settings or auth passthrough. Ensure it launches correctly with auth, settings, and interview prompt for all of the `new *` commands
