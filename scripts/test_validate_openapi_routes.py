from __future__ import annotations

import unittest
from pathlib import Path

from scripts import validate_openapi_routes as validator


ROOT = validator.ROOT


def make_path(name: str) -> Path:
    # Use a path under ROOT so relative_to works for error messages
    return ROOT / "openapi" / name


def minimal_doc(
    *,
    schemas: dict | None = None,
    parameters: dict | None = None,
    security_schemes: dict | None = None,
    paths: dict | None = None,
    global_security: list | None = None,
):
    doc: dict = {
        "openapi": "3.1.0",
        "info": {"title": "test", "version": "1.0.0"},
        "paths": paths or {},
    }
    if global_security is not None:
        doc["security"] = global_security
    components: dict = {}
    if schemas is not None:
        components["schemas"] = schemas
    if parameters is not None:
        components["parameters"] = parameters
    if security_schemes is not None:
        components["securitySchemes"] = security_schemes
    if components:
        doc["components"] = components
    return doc


class ValidateOpenApiRoutesReachabilityTests(unittest.TestCase):
    def test_unreachable_schema_is_detected(self) -> None:
        doc = minimal_doc(
            schemas={
                "Reachable": {"type": "string"},
                "Unreachable": {"type": "string"},
            },
            paths={
                "/v1/test": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": {"$ref": "#/components/schemas/Reachable"}
                                    }
                                }
                            }
                        }
                    }
                }
            },
        )
        errors = validator.check_component_reachability(make_path("test-unreachable-schema.yaml"), doc)
        self.assertEqual(errors, 1)

    def test_unreachable_parameter_is_detected(self) -> None:
        doc = minimal_doc(
            schemas={"UuidV7": {"type": "string"}},
            parameters={
                "UsedParam": {"name": "used", "in": "query", "schema": {"$ref": "#/components/schemas/UuidV7"}},
                "UnusedParam": {"name": "unused", "in": "query", "schema": {"type": "string"}},
            },
            paths={
                "/v1/test": {
                    "get": {
                        "parameters": [{"$ref": "#/components/parameters/UsedParam"}],
                        "responses": {"200": {"description": "ok"}},
                    }
                }
            },
        )
        errors = validator.check_component_reachability(make_path("test-unreachable-param.yaml"), doc)
        self.assertEqual(errors, 1)

    def test_unreachable_security_scheme_is_detected(self) -> None:
        doc = minimal_doc(
            security_schemes={
                "usedAuth": {"type": "http", "scheme": "bearer"},
                "unusedAuth": {"type": "http", "scheme": "bearer"},
            },
            global_security=[{"usedAuth": []}],
            paths={
                "/v1/test": {
                    "get": {
                        "responses": {"200": {"description": "ok"}},
                    }
                }
            },
        )
        errors = validator.check_component_reachability(make_path("test-unreachable-sec.yaml"), doc)
        self.assertEqual(errors, 1)

    def test_transitive_reference_is_followed(self) -> None:
        # A -> B -> C chain
        doc = minimal_doc(
            schemas={
                "A": {"properties": {"b": {"$ref": "#/components/schemas/B"}}},
                "B": {"properties": {"c": {"$ref": "#/components/schemas/C"}}},
                "C": {"type": "string"},
                "Unreachable": {"type": "string"},
            },
            paths={
                "/v1/test": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": {"$ref": "#/components/schemas/A"}
                                    }
                                }
                            }
                        }
                    }
                }
            },
        )
        errors = validator.check_component_reachability(make_path("test-transitive.yaml"), doc)
        # Only Unreachable should be flagged => 1 error (schema)
        self.assertEqual(errors, 1)

        # Now without the chain, C should also be unreachable => 2 errors? Actually schemas: A, B, C, Unreachable
        # If operation refs only A, and A refs B, but B does NOT ref C, then C is unreachable plus Unreachable = 2 schemas unreachable => 1 error bucket still counts as 1 for schemas? But we count errors per category.
        # Our function returns 1 per category with unreachable. So still 1. To test transitive, we check that removing B->C makes C unreachable but still same error count.
        # Better to test positive: with chain, reachable set should include C, so only Unreachable flagged.
        # Without chain, both C and Unreachable are flagged, but we can verify by checking that C is not flagged when chain exists.
        # We already verified chain exists gives 1. Now test broken chain: B without ref to C
        doc2 = minimal_doc(
            schemas={
                "A": {"properties": {"b": {"$ref": "#/components/schemas/B"}}},
                "B": {"type": "string"},  # no ref to C
                "C": {"type": "string"},
                "Unreachable": {"type": "string"},
            },
            paths={
                "/v1/test": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": {"$ref": "#/components/schemas/A"}
                                    }
                                }
                            }
                        }
                    }
                }
            },
        )
        # Now reachable are A, B. Unreachable are C, Unreachable => still 1 error (schema category)
        # To make test more precise, we check that with chain C is considered reachable by ensuring that adding C to unreachable list would be false
        # Instead we verify that a doc with only A->B->C and no extra Unreachable has 0 errors when chain is intact
        doc3 = minimal_doc(
            schemas={
                "A": {"properties": {"b": {"$ref": "#/components/schemas/B"}}},
                "B": {"properties": {"c": {"$ref": "#/components/schemas/C"}}},
                "C": {"type": "string"},
            },
            paths={
                "/v1/test": {
                    "get": {
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": {"$ref": "#/components/schemas/A"}
                                    }
                                }
                            }
                        }
                    }
                }
            },
        )
        errors3 = validator.check_component_reachability(make_path("test-transitive-ok.yaml"), doc3)
        self.assertEqual(errors3, 0)

    def test_path_level_parameter_schema_is_considered_reachable(self) -> None:
        doc = minimal_doc(
            schemas={
                "UuidV7": {"type": "string", "pattern": "^[0-9a-f-]+$"},
                "UnusedSchema": {"type": "string"},
            },
            parameters={
                "UserId": {"name": "user_id", "in": "path", "required": True, "schema": {"$ref": "#/components/schemas/UuidV7"}},
            },
            paths={
                "/v1/users/{user_id}": {
                    "parameters": [{"$ref": "#/components/parameters/UserId"}],
                    "get": {
                        "responses": {"200": {"description": "ok"}},
                    },
                }
            },
        )
        errors = validator.check_component_reachability(make_path("test-path-level.yaml"), doc)
        # UnusedSchema should be flagged => 1 error
        self.assertEqual(errors, 1)
        # But if we remove UnusedSchema, should be 0 and UuidV7 must be considered reachable via path-level param
        doc2 = minimal_doc(
            schemas={
                "UuidV7": {"type": "string", "pattern": "^[0-9a-f-]+$"},
            },
            parameters={
                "UserId": {"name": "user_id", "in": "path", "required": True, "schema": {"$ref": "#/components/schemas/UuidV7"}},
            },
            paths={
                "/v1/users/{user_id}": {
                    "parameters": [{"$ref": "#/components/parameters/UserId"}],
                    "get": {
                        "responses": {"200": {"description": "ok"}},
                    },
                }
            },
        )
        errors2 = validator.check_component_reachability(make_path("test-path-level-ok.yaml"), doc2)
        self.assertEqual(errors2, 0)

    def test_global_security_scheme_is_considered_reachable(self) -> None:
        doc = minimal_doc(
            security_schemes={
                "bearerAuth": {"type": "http", "scheme": "bearer"},
            },
            global_security=[{"bearerAuth": []}],
            paths={
                "/v1/test": {
                    "get": {
                        "responses": {"200": {"description": "ok"}},
                    }
                }
            },
        )
        errors = validator.check_component_reachability(make_path("test-global-sec-ok.yaml"), doc)
        self.assertEqual(errors, 0)

        doc2 = minimal_doc(
            security_schemes={
                "bearerAuth": {"type": "http", "scheme": "bearer"},
                "unusedScheme": {"type": "http", "scheme": "bearer"},
            },
            global_security=[{"bearerAuth": []}],
            paths={
                "/v1/test": {
                    "get": {
                        "responses": {"200": {"description": "ok"}},
                    }
                }
            },
        )
        errors2 = validator.check_component_reachability(make_path("test-global-sec-unused.yaml"), doc2)
        self.assertEqual(errors2, 1)


if __name__ == "__main__":
    unittest.main()
