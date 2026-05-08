/* Phase 3c spike wrapper header.
 * Bindgen reads this single header to find every WinFsp type / fn we need.
 *
 * <windows.h> must come BEFORE <winfsp/winfsp.h>; WinFsp depends on
 * Windows-defined types (PWSTR, BOOLEAN, ULONG, ...).
 *
 * NTSTATUS / PNTSTATUS are not pulled in by plain <windows.h> with
 * WIN32_LEAN_AND_MEAN; they live in <ntstatus.h> / <bcrypt.h>. We
 * provide both via direct typedefs (NTSTATUS is just `LONG`) before
 * including the WinFsp header to avoid SDK-include-order subtleties.
 */

#define WIN32_LEAN_AND_MEAN
#include <windows.h>

#ifndef _NTDEF_
typedef LONG NTSTATUS;
typedef NTSTATUS *PNTSTATUS;
#endif

#include <winfsp/winfsp.h>
