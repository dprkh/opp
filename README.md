`opp` is a macOS-only local broker for user-authorized 1Password CLI access. It retains one background terminal so independent local processes can reuse authorization.

### Install or upgrade

```sh
curl -L --proto '=https' --tlsv1.2 -sSf https://github.com/dprkh/opp/releases/latest/download/install.sh | sh
```

### Use

```sh
opp start
opp exec -- vault list --format=json
opp status
opp stop
```
