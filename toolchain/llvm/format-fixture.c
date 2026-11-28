//J960_r26_001
// Host-produced foreign-format objects; the native proof reads rather than encodes them.
extern int external(int);
int sh9_global = 42;
int sh9_function(int input) { return external(input) + sh9_global; }
