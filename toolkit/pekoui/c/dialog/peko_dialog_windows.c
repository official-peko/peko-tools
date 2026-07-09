/*
 * peko_dialog_windows.c
 *
 * Windows folder chooser for pekoui::dialog, backed by IFileOpenDialog with the
 * pick-folders option. COM is initialized on the calling thread as a
 * single-threaded apartment. The dialog blocks, so the caller parks for the GC.
 * The COM GUIDs are defined in this translation unit (INITGUID) so no uuid
 * import library is needed. Compiled only for Windows.
 */

#if defined(_WIN32)

#define INITGUID
#include <initguid.h>
#include <windows.h>
#include <shobjidl.h>
#include <string.h>

/* The runtime parks and unparks the calling thread across the blocking wait. */
extern void pgc_begin_blocking(void);
extern void pgc_end_blocking(void);

static char g_dialog_path[4096];

const char *peko_dialog_pick_folder(const char *title)
{
    (void)title;
    g_dialog_path[0] = '\0';

    pgc_begin_blocking();

    HRESULT hr = CoInitializeEx(NULL, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    int initialized = SUCCEEDED(hr) || hr == RPC_E_CHANGED_MODE;

    IFileOpenDialog *dialog = NULL;
    if (SUCCEEDED(CoCreateInstance(&CLSID_FileOpenDialog, NULL, CLSCTX_INPROC_SERVER,
                                   &IID_IFileOpenDialog, (void **)&dialog)))
    {
        DWORD options = 0;
        dialog->lpVtbl->GetOptions(dialog, &options);
        dialog->lpVtbl->SetOptions(dialog, options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM);

        if (SUCCEEDED(dialog->lpVtbl->Show(dialog, NULL)))
        {
            IShellItem *item = NULL;
            if (SUCCEEDED(dialog->lpVtbl->GetResult(dialog, &item)))
            {
                PWSTR wide = NULL;
                if (SUCCEEDED(item->lpVtbl->GetDisplayName(item, SIGDN_FILESYSPATH, &wide)))
                {
                    WideCharToMultiByte(CP_UTF8, 0, wide, -1, g_dialog_path,
                                        (int)sizeof(g_dialog_path) - 1, NULL, NULL);
                    CoTaskMemFree(wide);
                }
                item->lpVtbl->Release(item);
            }
        }
        dialog->lpVtbl->Release(dialog);
    }

    if (initialized)
    {
        CoUninitialize();
    }

    pgc_end_blocking();
    return g_dialog_path;
}

#endif /* _WIN32 */
