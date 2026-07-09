#include <peko.h>

PEKO_BEGIN

/* Native file dialogs for pekoui::dialog.

   pick_folder opens the OS folder chooser and returns the selected absolute
   path, or the empty string when the user cancels or no chooser is wired for
   the platform. The result is a static buffer the caller copies into managed
   memory right away. The call blocks while the dialog is open and runs the
   panel on the UI thread, so the caller parks for the GC. Defined per platform
   in c/dialog/peko_dialog_apple.m and c/dialog/peko_dialog_fallback.c. */
p_fn p_gcsafe p_cstr peko_dialog_pick_folder(p_gc(p_i8) title);

PEKO_END
