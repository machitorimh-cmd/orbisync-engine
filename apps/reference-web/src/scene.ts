import * as T from "three";
import { BEACON, STAR_POSITIONS } from "./game";

export function makeScene(container: HTMLElement) {
    const scene = new T.Scene();
    scene.background = new T.Color("#b9dadb");
    scene.fog = new T.Fog("#b9dadb", 45, 100);
    const renderer = new T.WebGLRenderer({
        antialias: true,
        powerPreference: "high-performance",
    });
    renderer.setPixelRatio(Math.min(devicePixelRatio, 1.75));
    renderer.shadowMap.enabled = true;
    renderer.shadowMap.type = T.PCFSoftShadowMap;
    renderer.outputColorSpace = T.SRGBColorSpace;
    renderer.toneMapping = T.ACESFilmicToneMapping;
    renderer.toneMappingExposure = 1.35;
    container.append(renderer.domElement);
    const camera = new T.OrthographicCamera(-20, 20, 15, -15, 0.1, 140);
    camera.position.set(0, 19, 30);
    scene.add(new T.HemisphereLight("#fff8dc", "#789b9c", 2.3));
    const sun = new T.DirectionalLight("#fff3ce", 3.4);
    sun.position.set(-14, 28, 12);
    sun.castShadow = true;
    sun.shadow.mapSize.set(1024, 1024);
    Object.assign(sun.shadow.camera, {
        left: -19,
        right: 19,
        top: 19,
        bottom: -19,
        near: 1,
        far: 70,
    });
    sun.shadow.bias = -0.0005;
    sun.shadow.normalBias = 0.04;
    scene.add(sun);
    const materials = new Map<string, T.MeshStandardMaterial>();
    function mat(color: string) {
        if (!materials.has(color))
            materials.set(
                color,
                new T.MeshStandardMaterial({
                    color,
                    roughness: 0.92,
                    flatShading: true,
                }),
            );
        return materials.get(color)!;
    }
    function mesh(
        geo: T.BufferGeometry,
        color: string,
        x = 0,
        y = 0,
        z = 0,
        parent: T.Object3D = scene,
    ) {
        const m = new T.Mesh(geo, mat(color));
        m.position.set(x, y, z);
        m.castShadow = true;
        m.receiveShadow = true;
        parent.add(m);
        return m;
    }
    function cylinder(
        rt: number,
        rb: number,
        h: number,
        color: string,
        x: number,
        y: number,
        z: number,
        segments = 12,
        parent: T.Object3D = scene,
    ) {
        return mesh(
            new T.CylinderGeometry(rt, rb, h, segments),
            color,
            x,
            y,
            z,
            parent,
        );
    }
    // Faceted suspended island, layered soil and moss.
    cylinder(12, 11.2, 1.2, "#77955e", 0, -0.62, 0, 48);
    cylinder(11.2, 8, 2.6, "#8b8570", 0, -2.5, 0, 13);
    cylinder(8, 3.3, 3.4, "#6e7c73", 0, -5.4, 0, 9);
    cylinder(3.3, 0, 2.8, "#62766e", 0, -8.4, 0, 7);
    cylinder(11.9, 11.9, 0.15, "#b6c685", 0, 0.015, 0, 64);
    const random = (() => {
        let s = 831;
        return () => {
            s = (s * 16807) % 2147483647;
            return (s - 1) / 2147483646;
        };
    })();
    // Pale stepping stones lead between the stars and the lighthouse.
    for (let i = 0; i < 65; i++) {
        const a = (i / 65) * Math.PI * 2;
        const m = cylinder(
            0.27 + random() * 0.18,
            0.35,
            0.07,
            i % 3 ? "#d6d1a2" : "#e4dcb5",
            Math.sin(a) * 8,
            0.13,
            Math.cos(a) * 8,
            6,
        );
        m.rotation.y = random() * 6;
    }
    for (let i = 0; i < 8; i++)
        cylinder(
            0.36,
            0.39,
            0.09,
            "#e4dcb5",
            Math.sin(i) * 0.25,
            0.14,
            1.4 + i * 0.68,
            6,
        );
    function tree(x: number, z: number, s: number, color: string) {
        const g = new T.Group();
        g.position.set(x, 0, z);
        g.scale.setScalar(s);
        scene.add(g);
        cylinder(0.14, 0.24, 1.8, "#8f8264", 0, 0.9, 0, 7, g);
        for (let i = 0; i < 3; i++) {
            const crown = mesh(
                new T.IcosahedronGeometry(1.2 - i * 0.15, 0),
                color,
                (i % 2) * 0.25,
                2 + i * 0.65,
                0,
                g,
            );
            crown.scale.set(1, 0.9, 1);
        }
        return g;
    }
    [
        [-9, 2, 1],
        [-8, -7, 1.2],
        [-4, -9, 0.85],
        [4, -9, 1.1],
        [9, -6, 0.9],
        [9, 3, 1],
        [-9, -3, 0.75],
        [4, 9, 0.7],
    ].forEach(([x, z, s], i) => tree(x, z, s, i % 2 ? "#79a27c" : "#4c8c78"));
    for (let i = 0; i < 160; i++) {
        const a = random() * Math.PI * 2,
            r = Math.sqrt(random()) * 10.9,
            x = Math.cos(a) * r,
            z = Math.sin(a) * r;
        if (
            Math.hypot(x, z + 1) < 2.4 ||
            STAR_POSITIONS.some(([sx, sz]) => Math.hypot(x - sx, z - sz) < 1.4)
        )
            continue;
        const c = i % 5 === 0 ? "#f5d985" : i % 3 === 0 ? "#f7eed0" : "#91ad71";
        mesh(new T.ConeGeometry(0.05, 0.18 + random() * 0.2, 4), c, x, 0.15, z);
    }
    [
        [-9, -1],
        [9, 1],
    ].forEach(([x, z]) => {
        const rock = mesh(
            new T.DodecahedronGeometry(0.95, 0),
            "#a3af96",
            x,
            0.5,
            z,
        );
        rock.scale.y = 0.8;
    });
    // Lighthouse with a dark glass lantern that becomes a warm beacon on completion.
    const tower = new T.Group();
    tower.position.set(BEACON.x, 0, BEACON.z);
    scene.add(tower);
    cylinder(1.9, 2.1, 0.3, "#d9d4b4", 0, 0.18, 0, 12, tower);
    cylinder(1.15, 1.45, 0.45, "#ede6ce", 0, 0.5, 0, 12, tower);
    cylinder(0.72, 1.07, 3.6, "#f5ecd5", 0, 2.5, 0, 12, tower);
    cylinder(0.84, 0.92, 0.3, "#86a698", 0, 3.25, 0, 12, tower);
    cylinder(1.1, 1, 0.18, "#d5c7a1", 0, 4.35, 0, 12, tower);
    const lantern = mesh(
        new T.CylinderGeometry(0.76, 0.76, 1.15, 12),
        "#638e88",
        0,
        4.95,
        0,
        tower,
    );
    const lanternMaterial = new T.MeshStandardMaterial({
        color: "#9bbdb0",
        emissive: "#ffc95e",
        emissiveIntensity: 0,
        roughness: 0.35,
    });
    lantern.material = lanternMaterial;
    for (let i = 0; i < 6; i++) {
        const a = (i * Math.PI) / 3;
        cylinder(
            0.055,
            0.055,
            1.2,
            "#49776c",
            Math.sin(a) * 0.77,
            4.95,
            Math.cos(a) * 0.77,
            6,
            tower,
        );
    }
    cylinder(0, 1.25, 0.9, "#537f71", 0, 5.98, 0, 12, tower);
    mesh(new T.SphereGeometry(0.14, 8, 8), "#ddbd68", 0, 6.52, 0, tower);
    const door = mesh(
        new T.BoxGeometry(0.5, 0.9, 0.12),
        "#557c6e",
        0,
        0.94,
        1,
        tower,
    );
    void door;
    const glow = new T.PointLight("#ffc66b", 0, 22, 1.5);
    glow.position.set(0, 5, -1);
    scene.add(glow);
    const beam = mesh(
        new T.CylinderGeometry(0.3, 3.3, 26, 32, 1, true),
        "#ffe4a0",
        0,
        18,
        -1,
    );
    beam.material = new T.MeshStandardMaterial({
        color: "#ffe4a0",
        transparent: true,
        opacity: 0.12,
        side: T.DoubleSide,
        depthWrite: false,
    });
    beam.visible = false;
    beam.castShadow = false;
    const stars = STAR_POSITIONS.map(([x, z]) => {
        const g = new T.Group();
        g.position.set(x, 0, z);
        scene.add(g);
        cylinder(0.55, 0.7, 0.18, "#d8d1a9", 0, 0.18, 0, 8, g);
        const star = mesh(
            new T.OctahedronGeometry(0.42, 0),
            "#ffce60",
            0,
            1.05,
            0,
            g,
        );
        star.material = new T.MeshStandardMaterial({
            color: "#ffd77c",
            emissive: "#ffb52d",
            emissiveIntensity: 0.45,
            metalness: 0.25,
            roughness: 0.3,
        });
        const ring = mesh(
            new T.TorusGeometry(0.7, 0.024, 6, 36),
            "#ffe4a1",
            0,
            0.26,
            0,
            g,
        );
        ring.rotation.x = -Math.PI / 2;
        return { g, star, ring };
    });
    const clouds: T.Group[] = [];
    for (let i = 0; i < 19; i++) {
        const g = new T.Group();
        scene.add(g);
        const a = (i / 19) * Math.PI * 2,
            r = 18 + random() * 15;
        g.position.set(Math.cos(a) * r, -4 - random() * 6, Math.sin(a) * r);
        for (let j = 0; j < 4; j++) {
            const puff = mesh(
                new T.IcosahedronGeometry(1.4 + random() * 1.5, 2),
                "#e8f1e7",
                j * 1.5,
                random() * 0.5,
                0,
                g,
            );
            puff.scale.set(1.8, 0.55, 1);
            puff.castShadow = false;
        }
        clouds.push(g);
    }
    function avatar(color = "#e7aa63") {
        const g = new T.Group();
        scene.add(g);
        const body = mesh(
            new T.CapsuleGeometry(0.24, 0.38, 4, 8),
            color,
            0,
            0.63,
            0,
            g,
        );
        const head = mesh(
            new T.SphereGeometry(0.26, 12, 10),
            "#fbdfab",
            0,
            1.17,
            0,
            g,
        );
        mesh(
            new T.SphereGeometry(0.285, 12, 8),
            "#416d65",
            0,
            1.28,
            -0.035,
            g,
        ).scale.set(1, 0.65, 1);
        for (const x of [-0.1, 0.1])
            mesh(
                new T.SphereGeometry(0.026, 6, 6),
                "#345347",
                x,
                1.18,
                0.24,
                g,
            );
        const bag = mesh(
            new T.BoxGeometry(0.33, 0.39, 0.22),
            "#5d8f80",
            0,
            0.68,
            -0.26,
            g,
        );
        const legs = [-0.14, 0.14].map((x) =>
            mesh(
                new T.CapsuleGeometry(0.09, 0.21, 3, 6),
                "#436d62",
                x,
                0.25,
                0,
                g,
            ),
        );
        void body;
        void head;
        void bag;
        return { g, legs };
    }
    const player = avatar();
    const pointer = mesh(
        new T.ConeGeometry(0.18, 0.46, 3),
        "#fff0b0",
        0,
        0.2,
        0,
    );
    pointer.geometry.rotateX(Math.PI / 2);
    pointer.castShadow = false;
    const motes = mesh(new T.IcosahedronGeometry(0.06, 0), "#fff2b5");
    motes.visible = false;
    const sparkGeo = new T.BufferGeometry();
    const sparkPositions = new Float32Array(90 * 3);
    for (let i = 0; i < 90; i++) {
        sparkPositions[i * 3] = (random() - 0.5) * 24;
        sparkPositions[i * 3 + 1] = random() * 8;
        sparkPositions[i * 3 + 2] = (random() - 0.5) * 24;
    }
    sparkGeo.setAttribute("position", new T.BufferAttribute(sparkPositions, 3));
    const sparkles = new T.Points(
        sparkGeo,
        new T.PointsMaterial({
            color: "#fff5c1",
            size: 0.1,
            transparent: true,
            opacity: 0.7,
        }),
    );
    scene.add(sparkles);
    let playing = false;
    function resize() {
        const aspect = innerWidth / innerHeight;
        const span = innerWidth < 700 ? Math.max(19, 13 / aspect) : 15;
        camera.left = -span * aspect;
        camera.right = span * aspect;
        camera.top = span;
        camera.bottom = -span;
        camera.updateProjectionMatrix();
        renderer.setSize(innerWidth, innerHeight);
    }
    resize();
    window.addEventListener("resize", resize);
    const look = new T.Vector3();
    function render(
        time: number,
        dt: number,
        state: {
            x: number;
            y: number;
            z: number;
            collected: Set<number>;
            complete: boolean;
        },
        moving: boolean,
        target: { x: number; z: number } | null,
        reduced: boolean,
    ) {
        look.lerp(
            new T.Vector3(
                playing ? 0 : innerWidth < 700 ? 0 : -5,
                innerWidth < 700 && !playing ? -4 : 0,
                0,
            ),
            1 - Math.exp(-dt * 3),
        );
        camera.lookAt(look);
        player.g.position.set(state.x, state.y + 0.1, state.z);
        player.legs.forEach(
            (leg, i) =>
                (leg.rotation.x =
                    moving && !reduced
                        ? Math.sin(time * 12 + i * Math.PI) * 0.6
                        : 0),
        );
        stars.forEach(({ g, star, ring }, i) => {
            g.visible = !state.collected.has(i);
            star.position.y =
                1.05 + (reduced ? 0 : Math.sin(time * 2 + i) * 0.16);
            star.rotation.y = reduced ? 0 : time * 0.7;
            ring.scale.setScalar(1 + (reduced ? 0 : Math.sin(time * 2) * 0.08));
        });
        lanternMaterial.emissiveIntensity = state.complete ? 2 : 0;
        glow.intensity = state.complete ? 22 : 0;
        beam.visible = state.complete;
        if (!reduced) {
            sparkles.rotation.y = time * 0.018;
            clouds.forEach(
                (c, i) =>
                    (c.position.y += Math.sin(time * 0.4 + i) * dt * 0.045),
            );
        }
        pointer.visible = playing && !!target && !state.complete;
        if (target) {
            const a = Math.atan2(target.x - state.x, target.z - state.z);
            pointer.position.set(
                state.x + Math.sin(a) * 0.85,
                0.22,
                state.z + Math.cos(a) * 0.85,
            );
            pointer.rotation.y = a;
        }
        renderer.render(scene, camera);
    }
    return {
        renderer,
        player,
        avatar,
        scene,
        render,
        setPlaying() {
            playing = true;
        },
    };
}
