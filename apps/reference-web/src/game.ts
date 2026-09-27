export const STAR_POSITIONS = [
    [-7, 4],
    [-7, -5],
    [0, -8],
    [7, -4],
    [7, 5],
] as const;
export const BEACON = { x: 0, z: -1 };
export const OBSTACLES = [
    { ...BEACON, radius: 1.65 },
    { x: -9, z: -1, radius: 1 },
    { x: 9, z: 1, radius: 1 },
];
export class Adventure {
    x = 0;
    z = 7;
    y = 0;
    velocityY = 0;
    elapsed = 0;
    started = false;
    complete = false;
    collected = new Set<number>();
    reset() {
        this.x = 0;
        this.z = 7;
        this.y = 0;
        this.velocityY = 0;
        this.elapsed = 0;
        this.complete = false;
        this.collected.clear();
        this.started = true;
    }
    get canIgnite() {
        return (
            this.collected.size === STAR_POSITIONS.length &&
            Math.hypot(this.x - BEACON.x, this.z - BEACON.z) < 3.4
        );
    }
    ignite() {
        if (!this.started || this.complete || !this.canIgnite) return false;
        this.complete = true;
        return true;
    }
    step(dt: number, dx: number, dz: number, run: boolean): number[] {
        if (!this.started) return [];
        dt = Math.min(Math.max(dt, 0), 0.05);
        if (!this.complete) this.elapsed += dt;
        const length = Math.hypot(dx, dz);
        if (length > 0) {
            const speed = run ? 5 : 3;
            const x = this.x + (dx / length) * speed * dt;
            const z = this.z + (dz / length) * speed * dt;
            const valid = (a: number, b: number) =>
                Math.hypot(a, b) < 11.25 &&
                OBSTACLES.every(
                    (o) => Math.hypot(a - o.x, b - o.z) > o.radius + 0.35,
                );
            if (valid(x, this.z)) this.x = x;
            if (valid(this.x, z)) this.z = z;
        }
        if (this.y > 0 || this.velocityY > 0) {
            this.velocityY -= 14 * dt;
            this.y = Math.max(0, this.y + this.velocityY * dt);
            if (!this.y) this.velocityY = 0;
        }
        const picked: number[] = [];
        if (!this.complete)
            STAR_POSITIONS.forEach(([x, z], i) => {
                if (
                    !this.collected.has(i) &&
                    Math.hypot(this.x - x, this.z - z) < 1.05
                ) {
                    this.collected.add(i);
                    picked.push(i);
                }
            });
        return picked;
    }
    jump() {
        if (this.started && this.y === 0) this.velocityY = 5.5;
    }
}
export function timeLabel(seconds: number) {
    return `${Math.floor(seconds / 60)
        .toString()
        .padStart(2, "0")}:${Math.floor(seconds % 60)
        .toString()
        .padStart(2, "0")}`;
}
