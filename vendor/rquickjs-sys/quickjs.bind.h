// A header which imports the all symbols of the quickjs header but also exports
// the static atoms.

#include "quickjs.h"

#if !defined(EMSCRIPTEN) && !defined(_MSC_VER)
#define CONFIG_ATOMICS
#endif

enum {
  __JS_ATOM_NULL = JS_ATOM_NULL,
#define DEF(name, str) JS_ATOM_##name,
#include "quickjs-atom.h"
#undef DEF
  JS_ATOM_END,
};
