Config files written by released versions, for the migration tests in
`src/config/mod.rs`. The layout is exactly what those versions wrote; every
value is made up (the UUIDs, the password hash of "test", the token and the
"sealed" passwords are fakes).

- `v1-2.1.8.json`: an earlier 2.1.x file (before remote names and accounts),
  the shape of a real install's file.
- `v1-2.1.10.json`: the last 2.1.x layout, signed in to an account, with
  names, stamps and saved host passwords.
