"""Negative controls for the ADR-007 production dependency boundary."""
import unittest
from scripts.check_architecture import extension_loader_errors, extension_source_errors


class ExtensionBoundaryTests(unittest.TestCase):
    def graph(self, loader, kind=None):
        names = {"s": "orbisync-server", "e": "orbisync-extensions", "l": loader}
        nodes = {
            "s": {"deps": [{"pkg": "e", "dep_kinds": [{"kind": None}]}]},
            "e": {"deps": [{"pkg": "l", "dep_kinds": [{"kind": kind}]}]},
            "l": {"deps": []},
        }
        return {"orbisync-server": "s"}, nodes, names

    def test_rejects_transitive_native_and_wasm_loaders(self):
        for loader in ("libloading", "dlopen2", "wasmtime", "wasmi", "wasmer", "extism"):
            with self.subTest(loader=loader):
                self.assertTrue(extension_loader_errors(*self.graph(loader)))

    def test_does_not_treat_build_or_test_tools_as_runtime(self):
        for kind in ("dev", "build"):
            self.assertEqual(extension_loader_errors(*self.graph("libloading", kind)), [])

    def test_out_of_process_http_transport_is_allowed(self):
        self.assertEqual(extension_loader_errors(*self.graph("reqwest")), [])

    def test_missing_server_fails_closed(self):
        self.assertTrue(extension_loader_errors({}, {}, {}))

    def test_raw_native_api_and_import_alias_are_rejected(self):
        for source in ('unsafe { dlopen(path, flags) }',
                       'use windows_sys::Win32::System::LibraryLoader::LoadLibraryW as load;',
                       'extern "C" { fn dlsym(); }'):
            self.assertTrue(extension_source_errors(source, "fixture.rs"))

    def test_http_api_does_not_trigger_native_guard(self):
        self.assertEqual(extension_source_errors('client.post(endpoint).await', "fixture.rs"), [])


if __name__ == "__main__":
    unittest.main()
