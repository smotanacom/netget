namespace py users

enum Role { ADMIN = 1, USER = 2 }

struct User {
  1: required i64 id,
  2: required string name,
  3: optional Role role,
  4: list<string> tags,
}

exception NotFound { 1: string message, 2: i64 id }

service Users {
  i32 add(1: i32 a, 2: i32 b),
  User get_user(1: i64 id) throws (1: NotFound missing),
  list<User> find(1: string prefix, 2: set<Role> roles),
  void touch(1: User u),
  oneway void ping(1: string note),
}
