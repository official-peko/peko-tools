/*
 * peko_dialog_linux.c
 *
 * Linux folder chooser for pekoui::dialog, backed by GtkFileChooserDialog. GTK
 * is not thread safe, so the dialog runs on the main loop through an idle
 * source; the caller (a bridge handler thread) parks for the GC and waits on a
 * condition until the main thread reports the result. Compiled only on desktop
 * Linux.
 */

#if defined(__linux__) && !defined(__ANDROID__)

#include <gtk/gtk.h>
#include <string.h>

/* The runtime parks and unparks the calling thread across the blocking wait. */
extern void pgc_begin_blocking(void);
extern void pgc_end_blocking(void);

static char g_dialog_path[4096];

typedef struct {
    const char *title;
    GMutex mutex;
    GCond cond;
    int done;
} pick_request;

/* Runs on the GTK main thread: show the chooser, copy the result, and signal. */
static gboolean pick_on_main(gpointer data)
{
    pick_request *request = (pick_request *)data;

    GtkWidget *dialog = gtk_file_chooser_dialog_new(
        request->title != NULL && request->title[0] != '\0' ? request->title : "Choose a Folder",
        NULL, GTK_FILE_CHOOSER_ACTION_SELECT_FOLDER,
        "_Cancel", GTK_RESPONSE_CANCEL,
        "_Open", GTK_RESPONSE_ACCEPT,
        NULL);

    if (gtk_dialog_run(GTK_DIALOG(dialog)) == GTK_RESPONSE_ACCEPT)
    {
        char *path = gtk_file_chooser_get_filename(GTK_FILE_CHOOSER(dialog));
        if (path != NULL)
        {
            strncpy(g_dialog_path, path, sizeof(g_dialog_path) - 1);
            g_dialog_path[sizeof(g_dialog_path) - 1] = '\0';
            g_free(path);
        }
    }
    gtk_widget_destroy(dialog);

    g_mutex_lock(&request->mutex);
    request->done = 1;
    g_cond_signal(&request->cond);
    g_mutex_unlock(&request->mutex);
    return G_SOURCE_REMOVE;
}

const char *peko_dialog_pick_folder(const char *title)
{
    g_dialog_path[0] = '\0';

    pick_request request;
    request.title = title;
    request.done = 0;
    g_mutex_init(&request.mutex);
    g_cond_init(&request.cond);

    /* Park for the GC while the main thread runs the dialog and this thread
       waits on the condition. */
    pgc_begin_blocking();
    gdk_threads_add_idle(pick_on_main, &request);
    g_mutex_lock(&request.mutex);
    while (!request.done)
    {
        g_cond_wait(&request.cond, &request.mutex);
    }
    g_mutex_unlock(&request.mutex);
    pgc_end_blocking();

    g_mutex_clear(&request.mutex);
    g_cond_clear(&request.cond);
    return g_dialog_path;
}

#endif /* __linux__ && !__ANDROID__ */
