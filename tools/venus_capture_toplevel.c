/* Optional host-side helper: return only the foreign-toplevel identifier of
 * the explicitly named Legion VM. Does not capture or list other windows. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wayland-client.h>
#include "venus-toplevel-client.h"

struct window { char *title; char *identifier; struct window *next; };
static struct window *windows;
static struct ext_foreign_toplevel_list_v1 *list;
static const char *wanted;
static void closed(void *data, struct ext_foreign_toplevel_handle_v1 *handle) {(void)data; (void)handle;}
static void done(void *data, struct ext_foreign_toplevel_handle_v1 *handle) {(void)data; (void)handle;}
static void title(void *data, struct ext_foreign_toplevel_handle_v1 *handle, const char *value) {
    (void)handle; struct window *window=data; free(window->title);
    window->title = strcmp(value,wanted)==0 ? strdup(value) : NULL;
}
static void app(void *data, struct ext_foreign_toplevel_handle_v1 *handle, const char *value) {(void)data; (void)handle; (void)value;}
static void identifier(void *data, struct ext_foreign_toplevel_handle_v1 *handle, const char *value) {
    (void)handle; struct window *window=data; free(window->identifier); window->identifier=strdup(value);
}
static const struct ext_foreign_toplevel_handle_v1_listener handle_listener={
    .closed=closed,.done=done,.title=title,.app_id=app,.identifier=identifier};
static void toplevel(void *data, struct ext_foreign_toplevel_list_v1 *object, struct ext_foreign_toplevel_handle_v1 *handle) {
    (void)data; (void)object; struct window *window=calloc(1,sizeof(*window));
    if (!window) exit(2);
    window->next=windows; windows=window;
    ext_foreign_toplevel_handle_v1_add_listener(handle,&handle_listener,window);
}
static void finished(void *data, struct ext_foreign_toplevel_list_v1 *object) {(void)data; (void)object;}
static const struct ext_foreign_toplevel_list_v1_listener list_listener={.toplevel=toplevel,.finished=finished};
static void global(void *data, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version) {
    (void)data; (void)version;
    if (strcmp(interface,"ext_foreign_toplevel_list_v1")==0) {
        list=wl_registry_bind(registry,name,&ext_foreign_toplevel_list_v1_interface,1);
        ext_foreign_toplevel_list_v1_add_listener(list,&list_listener,NULL);
    }
}
static void removed(void *data, struct wl_registry *registry, uint32_t name) {(void)data; (void)registry; (void)name;}
static const struct wl_registry_listener registry_listener={.global=global,.global_remove=removed};
int main(int argc,char **argv) {
    if (argc!=2) return 2;
    wanted=argv[1];
    struct wl_display *display=wl_display_connect(NULL);
    if (!display) return 2;
    struct wl_registry *registry=wl_display_get_registry(display);
    wl_registry_add_listener(registry,&registry_listener,NULL);
    for (int i=0;i<4;i++) if (wl_display_roundtrip(display)<0) return 2;
    int found=0;
    for (struct window *window=windows;window;window=window->next)
        if (window->title && window->identifier) {puts(window->identifier);found++;}
    wl_display_disconnect(display);
    return found==1?0:1;
}
