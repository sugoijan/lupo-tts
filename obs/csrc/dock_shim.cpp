// SPDX-License-Identifier: Apache-2.0
//
// Creates a native OBS dock hosting a CEF browser widget, using obs-browser's
// "panel" API (obs-browser/panel/browser-panel.hpp). That API is C++: an
// abstract `QCef` object whose virtual methods take `std::string`, and a
// `QWidget*` that goes straight into `obs_frontend_add_dock_by_id`. This shim
// is the only C++ in the plugin and touches no Qt headers: it mirrors the
// `QCef` vtable exactly as declared upstream and treats every Qt type as an
// opaque pointer. It must be kept in sync with browser-panel.hpp; the version
// exported by obs-browser is checked at run time.
//
// Call only on the OBS UI thread, after obs-browser has loaded
// (`obs_module_post_load` satisfies both).

#include <string>

extern "C" {
struct obs_module_t;
obs_module_t *obs_get_module(const char *name);
void *obs_get_module_lib(obs_module_t *module);
void *os_dlsym(void *module, const char *func);
bool obs_frontend_add_dock_by_id(const char *id, const char *title, void *widget);
void obs_frontend_remove_dock(const char *id);
}

class QObject;
class QCefWidget;
struct QCefCookieManager;

// --- Just enough Qt to give the dock a native window ------------------------
//
// OBS's own BrowserDock calls setAttribute(Qt::WA_NativeWindow) on the *dock*:
// on macOS the CEF view is a native NSView, and without a native dock it ends
// up attached at main-window level, ignoring tab stacking and painting over
// neighbouring docks. QWidget::setAttribute is an exported, non-virtual member
// (mangled name fixed by this declaration), and QObject::parent() is an inline
// read of QObjectData — a public, binary-stable layout in Qt 6.
namespace Qt {
enum WidgetAttribute { WA_NativeWindow = 100 };
}
class QWidget {
public:
	void setAttribute(Qt::WidgetAttribute attribute, bool on = true);
};
struct QObjectDataLayout {
	void *vptr;
	void *q_ptr;
	QObject *parent;
};
struct QObjectLayout {
	void *vptr;
	QObjectDataLayout *d_ptr;
};
static QWidget *qobject_parent_widget(void *object)
{
	QObjectLayout *layout = (QObjectLayout *)object;
	if (!layout || !layout->d_ptr)
		return nullptr;
	// QWidget derives from QObject first, so the addresses coincide.
	return (QWidget *)layout->d_ptr->parent;
}

// Mirror of `struct QCef` in browser-panel.hpp. Slot order is what matters;
// `get_cookie_path` really returns `BPtr<char>`, but that slot is never called
// from here so the declared return type is irrelevant to the layout.
struct QCef {
	virtual ~QCef() {}

	virtual bool init_browser(void) = 0;
	virtual bool initialized(void) = 0;
	virtual bool wait_for_browser_init(void) = 0;

	virtual QCefWidget *create_widget(QWidget *parent, const std::string &url,
					  QCefCookieManager *cookie_manager = nullptr) = 0;

	virtual QCefCookieManager *create_cookie_manager(const std::string &storage_path,
							 bool persist_session_cookies = false) = 0;

	virtual void *get_cookie_path(const std::string &storage_path) = 0;

	virtual void add_popup_whitelist_url(const std::string &url, QObject *obj) = 0;
	virtual void add_force_popup_url(const std::string &url, QObject *obj) = 0;
};

/// Oldest obs-browser panel API we accept (OBS 30/31 export 3).
static const int kMinQcefVersion = 3;

static QCef *g_cef = nullptr;

extern "C" int lupo_native_dock_qcef_version(void)
{
	obs_module_t *mod = obs_get_module("obs-browser");
	if (!mod)
		return -1;
	void *lib = obs_get_module_lib(mod);
	if (!lib)
		return -2;
	int (*version)(void) = (int (*)(void))os_dlsym(lib, "obs_browser_qcef_version_export");
	if (!version)
		return -3;
	return version();
}

/// Returns 0 on success; negative codes identify the failing step.
extern "C" int lupo_native_dock_create(const char *id, const char *title, const char *url)
{
	obs_module_t *mod = obs_get_module("obs-browser");
	if (!mod)
		return -1;
	void *lib = obs_get_module_lib(mod);
	if (!lib)
		return -2;
	int version = lupo_native_dock_qcef_version();
	if (version < 0)
		return -9; /* the version could not be queried at all */
	if (version < kMinQcefVersion)
		return -3;
	QCef *(*create_qcef)(void) = (QCef * (*)(void)) os_dlsym(lib, "obs_browser_create_qcef");
	if (!create_qcef)
		return -4;
	if (!g_cef)
		g_cef = create_qcef();
	if (!g_cef)
		return -5;
	// Same sequence as OBS's own InitBrowserPanelSafeBlock: init_browser()
	// returns true when CEF is already up, otherwise wait for it.
	if (!g_cef->init_browser() && !g_cef->wait_for_browser_init())
		return -6;
	QCefWidget *widget = g_cef->create_widget(nullptr, std::string(url), nullptr);
	if (!widget)
		return -7;
	if (!obs_frontend_add_dock_by_id(id, title, (void *)widget))
		return -8;
	// setWidget() reparented our widget to the OBSDock OBS just created.
	QWidget *dock = qobject_parent_widget(widget);
	if (!dock || (void *)dock == (void *)widget)
		return 1;
	dock->setAttribute(Qt::WA_NativeWindow, true);
	return 0;
}

extern "C" void lupo_native_dock_remove(const char *id)
{
	obs_frontend_remove_dock(id);
}
