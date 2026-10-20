// Generates crates/qbx_fivem_data/stubs/glm.lua from the registration tables of the CfxLua GLM binding.
// usage: node scripts/generate-glm-stub.mjs [folder-or-base-url-of-the-binding-sources]
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// citizenfx/lua `luaglm-548`, the revision FiveM builds its Lua runtime from.
const SOURCE = 'https://raw.githubusercontent.com/citizenfx/lua/4ee6aab505bca3346aa9dfafd8bc91bd14c85f1a/libs/glm-binding';
const base = (process.argv[2] ?? SOURCE).replace(/[\\/]$/, '');

/** The source without its comments, such as commented-out registrations; block comments keep their line breaks. */
const uncommented = (source) => {
    let out = '';
    for (let i = 0; i < source.length; i++) {
        const opener = source.slice(i, i + 2);
        if (source[i] === '"' || source[i] === "'") {
            let end = i + 1;
            while (end < source.length && source[end] !== source[i] && source[end] !== '\n') {
                end += source[end] === '\\' ? 2 : 1;
            }
            out += source.slice(i, end + 1);
            i = end;
        } else if (opener === '//' || opener === '/*') {
            const found = source.indexOf(opener === '//' ? '\n' : '*/', i + 2);
            const end = found < 0 ? source.length : opener === '//' ? found : found + 2;
            out += source.slice(i, end).replace(/[^\n]/g, '');
            i = end - 1;
        } else {
            out += source[i];
        }
    }
    return out;
};

/** The source without comments and without what only `_DEBUG` builds compile. */
const compiled = (source) => {
    const lines = [];
    /** For each open `#if`, whether it only holds `_DEBUG` code. */
    const debugOnly = [];
    for (const line of uncommented(source.replace(/\r\n/g, '\n')).split('\n')) {
        const directive = /^\s*#\s*(ifdef|ifndef|if|elif|else|endif)\b(.*)$/.exec(line);
        if (directive) {
            const [, keyword, condition] = directive;
            if (keyword === 'ifdef') {
                debugOnly.push(condition.trim() === '_DEBUG');
            } else if (keyword === 'if') {
                debugOnly.push(/^\s*defined\s*\(\s*_DEBUG\s*\)\s*$/.test(condition));
            } else if (keyword === 'ifndef') {
                debugOnly.push(false);
            } else if (keyword === 'endif') {
                debugOnly.pop();
            } else {
                debugOnly[debugOnly.length - 1] = false;
            }
        }
        if (!debugOnly.includes(true)) {
            lines.push(line);
        }
    }
    return lines.join('\n');
};
const read = async (file) =>
    compiled(/^https?:/.test(base) ? await (await fetch(`${base}/${file}`)).text() : readFileSync(join(base, file), 'utf8'));

const ENTRY = /\{\s*"([A-Za-z_]\w*)"\s*,\s*(\w+)(?:\((\w+)\))?\s*\}|GLM_LUA_REG\((\w+)\)/g;
const isPublic = (name) => !name.startsWith('__') && !name.startsWith('operator_');

/** name -> true for constants, false for functions */
const names = new Map();
/** alias -> the function it is another name of, e.g. tointeger -> toint and eulerXYZ -> eulerAngleXYZ */
const aliases = new Map();
for (const match of (await read('lglmlib_reg.hpp')).matchAll(ENTRY)) {
    const name = match[1] ?? match[4];
    if (names.has(name)) {
        continue;
    }
    names.set(name, match[2] === 'GLM_NULLPTR');
    if (match[2] === 'GLM_NAME') {
        aliases.set(name, match[3]);
    } else if (match[2]?.startsWith('glm_')) {
        aliases.set(name, match[2].slice('glm_'.length));
    }
}
if (names.size < 500) {
    throw new Error(`only ${names.size} names found in ${base}; the registration table moved or changed format`);
}

const library = await read('lglmlib.cpp');
const registers = (name) => library.includes(`"${name}"`);
const LIBRARY_NUMBERS = ['pi', 'eps', 'feps', 'huge'];
const LIBRARY_INTEGERS = ['maxinteger', 'mininteger', 'FP_INFINITE', 'FP_NAN', 'FP_ZERO', 'FP_SUBNORMAL', 'FP_NORMAL'];
[...LIBRARY_NUMBERS, ...LIBRARY_INTEGERS].filter(registers).forEach((name) => names.set(name, true));
['type', 'random', 'randomseed'].filter(registers).forEach((name) => names.set(name, false));

/** sub-library (glm.polygon, glm.aabb, ...) -> its function names */
const geometry = new Map();
const geom = await read('geom.hpp');
const registrations = [
    ...[...library.matchAll(/luaglm_(\w+?)lib\);\s*lua_setfield\(L, -2, "(\w+)"\);/g)].map((m) => [m[1], m[2]]),
    ...[...library.matchAll(/"(\w+)",\s*luaglm_(\w+?)lib\);/g)].map((m) => [m[2], m[1]]),
];
for (const [table, name] of registrations) {
    const body = new RegExp(`luaglm_${table}lib\\[\\]\\s*=\\s*\\{([\\s\\S]*?)\\n\\};`).exec(geom);
    if (body) {
        const members = [...body[1].matchAll(ENTRY)].map((m) => m[1] ?? m[4]).filter(isPublic);
        geometry.set(name, [...new Set(members)].sort((a, b) => a.localeCompare(b)));
    }
}
if (!geometry.has('polygon')) {
    throw new Error('the geometry libraries were not found; lglmlib.cpp or geom.hpp changed format');
}

// `T` stands for the generic of a signature, which takes the type of its argument; the runtime
// rejects arguments of different kinds, such as a vector3 with a vector2.
const NUM = 'number|vector2|vector3|vector4';
const V = 'glm.vec';
const VECTORS = ['vector3', 'vector2', 'vector4'];
const each = (doc, kinds = NUM) => ({ doc, generic: `T: ${kinds}`, params: [['x', 'T']], returns: 'T' });
const pair = (doc, [a, b], returns = 'T') => ({ doc, generic: `T: ${NUM}`, params: [[a, 'T'], [b, 'T']], returns });
const ctor = (type, doc) => ({ doc, params: [['...', 'number|glm.vec|quat|matrix']], returns: type });
const ease = { doc: 'Easing curve for `t` in the range 0..1.', params: [['t', 'number']], returns: 'number' };
const fun = (params, returns) => `fun(${params.map(([name, type]) => `${name}: ${type}`).join(', ')}): ${returns}`;
/**
 * A comparison that gives a boolean for numbers and a vector of 0s and 1s for vectors. `T` in
 * `params` is the type compared; a quaternion gives a vector4 when `quat` is set.
 */
const relational = (doc, params, { quat = false, scalar = 'number' } = {}) => {
    const typed = (type) => params.map(([name, param]) => [name, param === 'T' ? type : param]);
    const overloads = VECTORS.map((vector) => fun(typed(vector), vector));
    return { doc, params: typed(scalar), returns: 'boolean', overloads: quat ? [...overloads, fun(typed('quat'), 'vector4')] : overloads };
};
const euler = (letters) => (letters.length === new Set(letters).size ? [...letters].map((axis) => `angle${axis}`) : ['angle1', 'angle2', 'angle3']);

const SIGNATURES = {
    vec: ctor(V, 'Builds a vector whose size follows from the arguments.'),
    vec1: ctor('number', 'Builds a one component vector, which CfxLua represents as a number.'),
    vec2: ctor('vector2', 'Builds a vector2 from numbers and/or other vectors.'),
    vec3: ctor('vector3', 'Builds a vector3 from numbers and/or other vectors.'),
    vec4: ctor('vector4', 'Builds a vector4 from numbers and/or other vectors.'),
    ivec2: ctor('vector2', 'Builds a vector2 with every component truncated to an integer.'),
    ivec3: ctor('vector3', 'Builds a vector3 with every component truncated to an integer.'),
    ivec4: ctor('vector4', 'Builds a vector4 with every component truncated to an integer.'),
    quat: ctor('quat', 'Builds a quaternion from `w, x, y, z`, an angle and axis, two directions, or a rotation matrix.'),
    qua: ctor('quat', 'Alias of `glm.quat`.'),
    mat: ctor('matrix', 'Builds a matrix whose size follows from the arguments.'),
    mat2: ctor('matrix', 'Builds a 2x2 matrix.'),
    mat3: ctor('matrix', 'Builds a 3x3 matrix from numbers, column vectors or a quaternion.'),
    mat4: ctor('matrix', 'Builds a 4x4 matrix from numbers, column vectors or a quaternion.'),

    abs: each('Absolute value, per component.'),
    ceil: each('Rounds up, per component.'),
    floor: each('Rounds down, per component.'),
    round: each('Rounds to the nearest integer, per component.'),
    trunc: each('Drops the fractional part, per component.'),
    fract: each('Fractional part `x - floor(x)`, per component.'),
    sign: each('-1, 0 or 1 depending on the sign, per component.'),
    sqrt: each('Square root, per component, or of a quaternion.', `${NUM}|quat`),
    inversesqrt: each('`1 / sqrt(x)`, per component.'),
    cbrt: each('Cube root, per component.'),
    exp: each('Natural exponentiation, per component, or of a quaternion.', `${NUM}|quat`),
    exp2: each('`2 ^ x`, per component.'),
    log: { ...each('Natural logarithm, per component, or of a quaternion; with `base`, the logarithm in that base.', `${NUM}|quat`), params: [['x', 'T'], ['base?', 'T']] },
    log2: each('Base 2 logarithm, per component.'),
    log10: each('Base 10 logarithm, per component.'),
    sin: each('Sine of an angle in radians, per component.'),
    cos: each('Cosine of an angle in radians, per component.'),
    tan: each('Tangent of an angle in radians, per component.'),
    asin: each('Arc sine in radians, per component.'),
    acos: each('Arc cosine in radians, per component.'),
    sinh: each('Hyperbolic sine, per component.'),
    cosh: each('Hyperbolic cosine, per component.'),
    tanh: each('Hyperbolic tangent, per component.'),
    asinh: each('Inverse hyperbolic sine, per component.'),
    acosh: each('Inverse hyperbolic cosine, per component.'),
    atanh: each('Inverse hyperbolic tangent, per component.'),
    degrees: each('Converts radians to degrees, per component.'),
    radians: each('Converts degrees to radians, per component.'),
    saturate: each('Clamps to the range 0..1, per component.'),
    atan: { doc: 'Arc tangent in radians; with two arguments the quadrant follows from the signs of `y` and `x`.', generic: `T: ${NUM}`, params: [['y', 'T'], ['x?', 'T']], returns: 'T' },
    atan2: pair('Arc tangent of `y / x` in radians, using the signs to pick the quadrant.', ['y', 'x']),
    mod: pair('Modulus `x - y * floor(x / y)`, per component.', ['x', 'y']),
    fmod: pair('Remainder of `x / y` with the sign of `x`, per component.', ['x', 'y']),
    pow: { doc: '`base ^ exponent`, per component, or of a quaternion.', generic: `T: ${NUM}|quat`, params: [['base', 'T'], ['exponent', 'number']], returns: 'T' },
    min: { doc: 'Smallest of the arguments, per component. A vector can be compared with numbers.', generic: `T: ${NUM}`, params: [['x', 'T'], ['...', 'T|number']], returns: 'T' },
    max: { doc: 'Largest of the arguments, per component. A vector can be compared with numbers.', generic: `T: ${NUM}`, params: [['x', 'T'], ['...', 'T|number']], returns: 'T' },
    step: pair('0 where `x < edge`, otherwise 1, per component.', ['edge', 'x']),
    clamp: { doc: 'Restricts `x` to `minVal..maxVal`, per component; with one argument to 0..1.', generic: `T: ${NUM}`, params: [['x', 'T'], ['minVal', 'T|number'], ['maxVal', 'T|number']], returns: 'T', overloads: [fun([['x', 'number']], 'number')] },
    mix: { doc: 'Linear blend `x * (1 - a) + y * a`. Quaternions are blended spherically.', generic: `T: ${NUM}|quat`, params: [['x', 'T'], ['y', 'T'], ['a', 'number|T']], returns: 'T' },
    lerp: { doc: 'Linear interpolation between `x` and `y`.', generic: `T: ${NUM}|quat`, params: [['x', 'T'], ['y', 'T'], ['a', 'number|T']], returns: 'T' },
    slerp: { doc: 'Spherical interpolation between two quaternions or directions.', generic: `T: ${NUM}|quat`, params: [['x', 'T'], ['y', 'T'], ['a', 'number']], returns: 'T' },
    smoothstep: { doc: 'Hermite interpolation between 0 and 1 while `x` moves from `edge0` to `edge1`.', generic: `T: ${NUM}`, params: [['edge0', 'T'], ['edge1', 'T'], ['x', 'T']], returns: 'T' },

    dot: pair('Dot product.', ['x', 'y'], 'number'),
    cross: {
        doc: 'Cross product of two vector3s or two quaternions. A quaternion and a vector3 give the vector3 rotated by the quaternion, or by its inverse when the vector3 comes first; two vector2s give a number.',
        params: [['x', 'vector3'], ['y', 'vector3']],
        returns: 'vector3',
        overloads: [fun([['x', 'quat'], ['y', 'quat']], 'quat'), fun([['q', 'quat'], ['v', 'vector3']], 'vector3'), fun([['v', 'vector3'], ['q', 'quat']], 'vector3'), fun([['x', 'vector2'], ['y', 'vector2']], 'number')],
    },
    length: { doc: 'Length of a vector, or the absolute value of a number.', params: [['x', NUM]], returns: 'number' },
    length2: { doc: 'Squared length, which avoids the square root.', params: [['x', NUM]], returns: 'number' },
    distance: pair('Distance between two points.', ['p0', 'p1'], 'number'),
    distance2: pair('Squared distance between two points.', ['p0', 'p1'], 'number'),
    normalize: each('Same direction, length 1.'),
    reflect: pair('Reflects the incident vector `I` around the normal `N`.', ['I', 'N']),
    refract: { doc: 'Refracts `I` through a surface with normal `N` and ratio of indices `eta`.', generic: `T: ${NUM}`, params: [['I', 'T'], ['N', 'T'], ['eta', 'number']], returns: 'T' },
    faceforward: { doc: 'Returns `N` flipped so that it faces against `I`.', generic: `T: ${NUM}`, params: [['N', 'T'], ['I', 'T'], ['Nref', 'T']], returns: 'T' },
    angle: { doc: 'Angle in radians between two normalized vectors, or the rotation angle of a quaternion.', generic: `T: ${NUM}|quat`, params: [['x', 'T'], ['y', 'T']], returns: 'number', overloads: [fun([['q', 'quat']], 'number')] },

    toint: { doc: 'Converts to an integer when the value has an exact integer representation, otherwise nil; vectors are truncated per component.', params: [['x', 'any']], returns: 'integer?', overloads: VECTORS.map((vector) => fun([['x', vector]], vector)) },
    all: { doc: 'True when every component is truthy.', params: [['v', 'any']], returns: 'boolean' },
    any: { doc: 'True when at least one component is truthy.', params: [['v', 'any']], returns: 'boolean' },
    isnan: relational('Whether the value is NaN, per component.', [['x', 'T']], { quat: true }),
    isinf: relational('Whether the value is infinite, per component.', [['x', 'T']], { quat: true }),
    isfinite: relational('Whether the value is finite, per component.', [['x', 'T']]),
    not_: relational('Logical negation, per component.', [['x', 'T']], { scalar: 'boolean|number' }),
    equal: relational('Component-wise equality, optionally within `epsilon`. Two floats need `epsilon`.', [['x', 'T'], ['y', 'T'], ['epsilon?', 'number']], { quat: true }),
    notEqual: relational('Component-wise inequality, optionally outside `epsilon`. Two floats need `epsilon`.', [['x', 'T'], ['y', 'T'], ['epsilon?', 'number']], { quat: true }),
    epsilonEqual: relational('Whether `|x - y| < epsilon`, per component.', [['x', 'T'], ['y', 'T'], ['epsilon?', 'number']]),
    epsilonNotEqual: relational('Whether `|x - y| >= epsilon`, per component.', [['x', 'T'], ['y', 'T'], ['epsilon?', 'number']]),
    lessThan: relational('Whether `x < y`, per component.', [['x', 'T'], ['y', 'T']], { quat: true }),
    lessThanEqual: relational('Whether `x <= y`, per component.', [['x', 'T'], ['y', 'T']], { quat: true }),
    greaterThan: relational('Whether `x > y`, per component.', [['x', 'T'], ['y', 'T']], { quat: true }),
    greaterThanEqual: relational('Whether `x >= y`, per component.', [['x', 'T'], ['y', 'T']], { quat: true }),
    unpack: { doc: 'Returns the components of vectors and quaternions, and the columns of matrices, as separate values. Other values are returned as they are.', params: [['...', 'any']], returns: 'any ...' },
    hash: { doc: 'Hash of each argument.', params: [['...', 'any']], returns: 'integer ...' },
    to_string: { doc: 'Human readable text for a vector, quaternion or matrix.', params: [['x', 'any']], returns: 'string' },
    up: { doc: 'The unit vector pointing up.', params: [], returns: 'vector3' },
    right: { doc: 'The unit vector pointing right.', params: [], returns: 'vector3' },
    forward: { doc: 'The unit vector pointing forward.', params: [], returns: 'vector3' },
    type: { doc: 'Returns "integer", "float", or nil when `x` is not a number.', params: [['x', 'any']], returns: '"integer"|"float"|nil kind' },
    random: { doc: 'With no arguments returns a float in [0, 1); with `m` an integer in [1, m]; with `m` and `n` an integer in [m, n]. `math.random(0)` yields a fully random integer.', params: [['m', 'integer'], ['n', 'integer']], returns: 'integer result', overloads: ['fun(): number', 'fun(m: integer): integer'] },
    randomseed: { doc: 'Seeds the pseudo-random generator, with a reasonably random seed when called without arguments, and returns the two parts of the seed.', params: [['x?', 'integer'], ['y?', 'integer']], returns: ['integer x', 'integer y'] },

    modf: { doc: 'Integral and fractional parts, per component.', generic: `T: ${NUM}`, params: [['x', 'T']], returns: ['T', 'T'] },
    frexp: { doc: 'Significand in the range 0.5..1 and exponent of two, per component.', params: [['x', 'number']], returns: ['number', 'integer'], overloads: VECTORS.map((vector) => `fun(x: ${vector}): ${vector}, ${vector}`) },
    sincos: { doc: 'Sine and cosine of an angle in radians, per component.', generic: `T: ${NUM}`, params: [['x', 'T']], returns: ['T', 'T'] },

    conjugate: { doc: 'Conjugate of a quaternion.', params: [['q', 'quat']], returns: 'quat' },
    inverse: each('Inverse of a quaternion or matrix, or `1 / x` per component.', `${NUM}|quat|matrix`),
    axis: { doc: 'Rotation axis of a quaternion.', params: [['q', 'quat']], returns: 'vector3' },
    angleAxis: { doc: 'Quaternion that rotates by `angle` radians around `axis`.', params: [['angle', 'number'], ['axis', 'vector3']], returns: 'quat' },
    eulerAngles: { doc: 'Pitch, yaw and roll of a quaternion, in radians.', params: [['q', 'quat']], returns: 'vector3' },
    pitch: { doc: 'Pitch of a quaternion in radians.', params: [['q', 'quat']], returns: 'number' },
    yaw: { doc: 'Yaw of a quaternion in radians.', params: [['q', 'quat']], returns: 'number' },
    roll: { doc: 'Roll of a quaternion in radians.', params: [['q', 'quat']], returns: 'number' },
    quatLookAt: { doc: 'Quaternion that looks along `direction` with the given `up` vector.', params: [['direction', 'vector3'], ['up', 'vector3']], returns: 'quat' },
    quat_identity: { doc: 'The quaternion that does not rotate.', params: [], returns: 'quat' },
    rotation: { doc: 'Quaternion that rotates the direction `orig` onto `dest`.', params: [['orig', 'vector3'], ['dest', 'vector3']], returns: 'quat' },
    rotateFromTo: { doc: 'Quaternion that rotates the direction `from` onto `to`.', params: [['from', 'vector3'], ['to', 'vector3']], returns: 'quat' },
    rotate: {
        doc: 'Rotates a vector, quaternion or matrix by `angle` radians around `axis`; a vector2 needs no axis. With an angle and an axis alone, builds the rotation matrix; with a quaternion and a vector, rotates the vector.',
        generic: 'T: quat|matrix|vector2|vector3|vector4',
        params: [['x', 'T'], ['angle', 'number'], ['axis?', 'vector3']],
        returns: 'T',
        overloads: [fun([['angle', 'number'], ['axis', 'vector3']], 'matrix'), fun([['q', 'quat'], ['v', 'vector3']], 'vector3'), fun([['q', 'quat'], ['v', 'vector4']], 'vector4')],
    },
    rotateX: { doc: 'Rotates a vector around the X axis by `angle` radians.', generic: 'T: vector3|vector4', params: [['v', 'T'], ['angle', 'number']], returns: 'T' },
    rotateY: { doc: 'Rotates a vector around the Y axis by `angle` radians.', generic: 'T: vector3|vector4', params: [['v', 'T'], ['angle', 'number']], returns: 'T' },
    rotateZ: { doc: 'Rotates a vector around the Z axis by `angle` radians.', generic: 'T: vector3|vector4', params: [['v', 'T'], ['angle', 'number']], returns: 'T' },
    mat3_cast: { doc: 'Rotation matrix of a quaternion.', params: [['q', 'quat']], returns: 'matrix' },
    mat4_cast: { doc: 'Rotation matrix of a quaternion.', params: [['q', 'quat']], returns: 'matrix' },
    toMat3: { doc: 'Rotation matrix of a quaternion.', params: [['q', 'quat']], returns: 'matrix' },
    toMat4: { doc: 'Rotation matrix of a quaternion.', params: [['q', 'quat']], returns: 'matrix' },
    quat_cast: { doc: 'Quaternion of a rotation matrix.', params: [['m', 'matrix']], returns: 'quat' },
    toQuat: { doc: 'Quaternion of a rotation matrix.', params: [['m', 'matrix']], returns: 'quat' },
    yawPitchRoll: { doc: 'Rotation matrix of a yaw, pitch and roll in radians.', params: [['yaw', 'number'], ['pitch', 'number'], ['roll', 'number']], returns: 'matrix' },
    decompose: { doc: 'Splits a transformation matrix into its parts, or returns nil when it cannot be decomposed.', params: [['m', 'matrix']], returns: ['vector3? scale', 'quat? orientation', 'vector3? translation', 'vector3? skew', 'vector4? perspective'] },
    axisAngle: { doc: 'Rotation axis and angle in radians of a rotation matrix.', params: [['m', 'matrix']], returns: ['vector3 axis', 'number angle'] },

    transpose: { doc: 'Transposed matrix.', params: [['m', 'matrix']], returns: 'matrix' },
    determinant: { doc: 'Determinant of a square matrix.', params: [['m', 'matrix']], returns: 'number' },
    identity: { doc: 'Identity matrix of `cols` columns and `rows` rows.', params: [['cols', 'integer'], ['rows', 'integer']], returns: 'matrix' },
    translate: { doc: 'Appends a translation to a matrix; with a vector alone, builds the translation matrix.', params: [['m', 'matrix'], ['v', 'vector3']], returns: 'matrix', overloads: [fun([['v', 'vector3']], 'matrix')] },
    scale: { doc: 'Appends a scale to a matrix; with a vector alone, builds the scale matrix.', params: [['m', 'matrix'], ['v', 'vector3']], returns: 'matrix', overloads: [fun([['v', 'vector3']], 'matrix')] },
    lookAt: { doc: 'View matrix for a camera at `eye` looking at `center`.', params: [['eye', 'vector3'], ['center', 'vector3'], ['up', 'vector3']], returns: 'matrix' },
    perspective: { doc: 'Perspective projection matrix; `fovy` is in radians.', params: [['fovy', 'number'], ['aspect', 'number'], ['near', 'number'], ['far', 'number']], returns: 'matrix' },
    ortho: { doc: 'Orthographic projection matrix.', params: [['left', 'number'], ['right', 'number'], ['bottom', 'number'], ['top', 'number'], ['near?', 'number'], ['far?', 'number']], returns: 'matrix' },
    rayPicking: { doc: 'Direction of the ray through the screen position `mouseX, mouseY` (0..1) of a camera; `fov` is in radians.', params: [['camDirection', 'vector3'], ['camUp', 'vector3'], ['fov', 'number'], ['aspectRatio', 'number'], ['zNear', 'number'], ['zFar', 'number'], ['mouseX', 'number'], ['mouseY', 'number']], returns: 'vector3' },

    intersectRayPlane: { doc: 'Distance along the ray to where it meets the plane, or nil when it misses.', generic: `T: ${NUM}`, params: [['orig', 'T'], ['dir', 'T'], ['planeOrig', 'T'], ['planeNormal', 'T']], returns: ['number? distance'] },
    intersectRaySphere: { doc: 'Position and normal where the ray meets the sphere, or nil when it misses.', generic: `T: ${NUM}`, params: [['rayStarting', 'T'], ['rayNormalizedDirection', 'T'], ['sphereCenter', 'T'], ['sphereRadius', 'number']], returns: ['T? position', 'T? normal'] },
    intersectLineSphere: { doc: 'Positions and normals where the line meets the sphere, or nil when it misses.', generic: `T: ${NUM}`, params: [['point0', 'T'], ['point1', 'T'], ['sphereCenter', 'T'], ['sphereRadius', 'number']], returns: ['T? position1', 'T? normal1', 'T? position2', 'T? normal2'] },
    intersectRayTriangle: { doc: 'Barycentric position and distance where the ray meets the triangle, or nil when it misses.', params: [['orig', 'vector3'], ['dir', 'vector3'], ['vert0', 'vector3'], ['vert1', 'vector3'], ['vert2', 'vector3']], returns: ['vector2? baryPosition', 'number? distance'] },
    smoothDamp: { doc: 'Moves `current` towards `target` like a spring, returning the new value and velocity.', generic: `T: ${NUM}`, params: [['current', 'T'], ['target', 'T'], ['currentVelocity', 'T'], ['smoothTime', 'number'], ['maxSpeed', 'number'], ['deltaTime', 'number']], returns: ['T value', 'T velocity'] },

    linearRand: pair('Uniformly distributed random value between `min` and `max`, per component.', ['min', 'max']),
    sphericalRand: { doc: 'Random point on a sphere of the given radius.', params: [['radius', 'number']], returns: 'vector3' },
    circularRand: { doc: 'Random point on a circle of the given radius.', params: [['radius', 'number']], returns: 'vector2' },
    ballRand: { doc: 'Random point inside a sphere of the given radius.', params: [['radius', 'number']], returns: 'vector3' },
    diskRand: { doc: 'Random point inside a circle of the given radius.', params: [['radius', 'number']], returns: 'vector2' },
};
for (const name of names.keys()) {
    if (/Ease(In|Out|InOut)$/.test(name) && !SIGNATURES[name]) {
        SIGNATURES[name] = ease;
    }
    const [, kind, letters] = /^(eulerAngle|quatEulerAngle|extractEulerAngle)([XYZ]{1,3})$/.exec(name) ?? [];
    if (kind === 'eulerAngle') {
        SIGNATURES[name] = { doc: `Rotation matrix of angles in radians around the axes ${[...letters].join(', ')}.`, params: euler(letters).map((p) => [p, 'number']), returns: 'matrix' };
    } else if (kind === 'quatEulerAngle') {
        SIGNATURES[name] = { doc: `Quaternion of angles in radians around the axes ${[...letters].join(', ')}.`, params: euler(letters).map((p) => [p, 'number']), returns: 'quat' };
    } else if (kind === 'extractEulerAngle') {
        SIGNATURES[name] = { doc: `Angles in radians around the axes ${[...letters].join(', ')} of a rotation matrix.`, params: [['m', 'matrix']], returns: ['number', 'number', 'number'] };
    }
}

/**
 * `signature` with a number signature first and an overload for each other kind its generic takes,
 * for functions whose result for a number is not of the number's own kind: a generic `T` would
 * give `glm.sqrt(4)` the `integer` of its argument, but the runtime returns a float for it, and an
 * integer from `glm.floor(1.5)`. `numbers` are the types returned for numbers, in order.
 */
const byKind = (signature, numbers) => {
    const kinds = signature.generic.replace(/^T:\s*/, '').split('|');
    const union = (types) => [...new Set(types.split('|'))].join('|');
    const typed = (type) => signature.params.map(([name, param]) => [name, union(param.replace(/\bT\b/g, type))]);
    const returned = (type) => [signature.returns].flat().map((ret) => ret.replace(/\bT\b/g, type).split(' ')[0]);
    const overloads = kinds.filter((kind) => kind !== 'number').map((kind) => fun(typed(kind), returned(kind).join(', ')));
    const { generic, ...rest } = signature;
    return { ...rest, params: typed('number'), returns: numbers, overloads: [...overloads, ...(signature.overloads ?? [])] };
};
// Floats for every number, as cfxlua.exe shows; abs, clamp, fabs, fmod, max, min, mod and pow keep
// the kind of their number, which their generic gives them.
const FLOAT_RESULTS = ['acos', 'acosh', 'asin', 'asinh', 'atan', 'atan2', 'atanh', 'cbrt', 'cos', 'cosh', 'degrees', 'exp', 'exp2', 'faceforward', 'fract', 'inverse', 'inversesqrt', 'lerp', 'linearRand', 'log', 'log10', 'log2', 'mix', 'normalize', 'radians', 'reflect', 'refract', 'round', 'saturate', 'sign', 'signbit', 'sin', 'sincos', 'sinh', 'slerp', 'smoothDamp', 'smoothstep', 'sqrt', 'step', 'tan', 'tanh', 'trunc'];
for (const name of FLOAT_RESULTS.filter((name) => SIGNATURES[name]?.generic)) {
    SIGNATURES[name] = byKind(SIGNATURES[name], [SIGNATURES[name].returns].flat().map((ret) => ret.replace(/\bT\b/, 'number')));
}
SIGNATURES.floor = byKind(SIGNATURES.floor, 'integer');
SIGNATURES.ceil = byKind(SIGNATURES.ceil, 'integer');
SIGNATURES.modf = byKind(SIGNATURES.modf, ['integer integral', 'number fractional']);

for (const [alias, target] of aliases) {
    if (SIGNATURES[target] && !SIGNATURES[alias]) {
        SIGNATURES[alias] = { ...SIGNATURES[target], doc: `Alias of \`glm.${target}\`. ${SIGNATURES[target].doc}` };
    }
}

const POLYGON = 'glm.polygon';
const GEOMETRY_SIGNATURES = {
    'polygon.new': { doc: 'Builds a polygon from its corner points.', params: [['points?', 'vector3[]']], returns: POLYGON },
    'polygon.contains': { doc: 'Whether `point` lies within the polygon, allowing `thickness` along its normal.', params: [['self', POLYGON], ['point', 'vector3'], ['thickness?', 'number']], returns: 'boolean' },
    'polygon.containsSegment': { doc: 'Whether the segment from `a` to `b` lies within the polygon, allowing `thickness` along its normal.', params: [['self', POLYGON], ['a', 'vector3'], ['b', 'vector3'], ['thickness?', 'number']], returns: 'boolean' },
    'polygon.area': { doc: 'Surface area of the polygon.', params: [['self', POLYGON]], returns: 'number' },
    'polygon.perimeter': { doc: 'Summed length of the edges.', params: [['self', POLYGON]], returns: 'number' },
    'polygon.centroid': { doc: 'Centre of mass of the polygon.', params: [['self', POLYGON]], returns: 'vector3' },
    'polygon.isConvex': { doc: 'Whether the polygon is convex.', params: [['self', POLYGON]], returns: 'boolean' },
    'polygon.isPlanar': { doc: 'Whether all points lie in one plane.', params: [['self', POLYGON]], returns: 'boolean' },
    'polygon.isSimple': { doc: 'Whether no two edges of the polygon cross.', params: [['self', POLYGON]], returns: 'boolean' },
    'segment.getPoint': { doc: 'Point at the fraction `t` (0..1) of the way from `a` to `b`.', params: [['a', 'vector3'], ['b', 'vector3'], ['t', 'number']], returns: 'vector3' },
    'segment2d.getPoint': { doc: 'Point at the fraction `t` (0..1) of the way from `a` to `b`.', params: [['a', 'vector2'], ['b', 'vector2'], ['t', 'number']], returns: 'vector2' },
};
/** Lines of the class of a geometry library whose values are of that class. */
const GEOMETRY_CLASSES = {
    polygon: [
        '---A polygon of vector3 points, built with `glm.polygon.new`. Indexing it gives a point and `#` the number of points.',
        `---@class ${POLYGON}`,
        '---@field [integer] vector3',
        `---@operator add(vector3): ${POLYGON}`,
        `---@operator sub(vector3): ${POLYGON}`,
        `---@operator unm: ${POLYGON}`,
        '---@operator len: integer',
    ],
};

const missing = [
    ...Object.keys(SIGNATURES).filter((name) => !names.has(name)),
    ...Object.keys(GEOMETRY_SIGNATURES).filter((key) => {
        const [library, member] = key.split('.');
        return !geometry.get(library)?.includes(member);
    }),
];
if (missing.length > 0) {
    throw new Error(`described but not registered by the binding: ${missing.join(', ')}`);
}

const lines = [
    '---@meta',
    '-- Generated by scripts/generate-glm-stub.mjs from the registration tables of the CfxLua GLM binding',
    `-- (${SOURCE}).`,
    '-- Functions without a description are registered by the runtime but not described here; they',
    '-- follow the GLM function of the same name.',
    '',
    '---A matrix of 2 to 4 columns and rows. Indexing it gives a column and `#` the number of columns.',
    '---@class matrix',
    '---@field [integer] vector2|vector3|vector4',
    '---@operator mul(matrix): matrix',
    '---@operator mul(vector4): vector4',
    '---@operator mul(vector3): vector3',
    '---@operator mul(vector2): vector2',
    `---@operator mul(${POLYGON}): ${POLYGON}`,
    '---@operator mul(number): matrix',
    '---@operator div(matrix): matrix',
    '---@operator div(number): matrix',
    '---@operator add(matrix): matrix',
    '---@operator add(number): matrix',
    '---@operator sub(matrix): matrix',
    '---@operator sub(number): matrix',
    '---@operator unm: matrix',
    '---@operator len: integer',
    '',
    '---@alias glm.vec vector2|vector3|vector4',
    '---@alias glm.num number|vector2|vector3|vector4',
    '',
    '---GLM mathematics for vectors, quaternions and matrices, built into CfxLua.',
    '---@class glm',
    'glm = {}',
    '',
];
const sorted = [...names.keys()].sort((a, b) => a.localeCompare(b));
for (const name of sorted.filter((n) => names.get(n))) {
    lines.push(`---@type ${LIBRARY_INTEGERS.includes(name) ? 'integer' : 'number'}`, `glm.${name} = 0`, '');
}
const KEYWORDS = new Set(['and', 'break', 'do', 'else', 'elseif', 'end', 'false', 'for', 'function', 'goto', 'if', 'in', 'local', 'nil', 'not', 'or', 'repeat', 'return', 'then', 'true', 'until', 'while']);
/** `function glm.name(args) end`, or `glm["name"] = function(args) end` for a name Lua reserves. */
const declaration = (owner, member, args) =>
    KEYWORDS.has(member) ? `${owner}["${member}"] = function(${args}) end` : `function ${owner}.${member}(${args}) end`;
const emit = (owner, member, signature) => {
    if (!signature) {
        lines.push('---@param ... any', '---@return any', declaration(owner, member, '...'), '');
        return;
    }
    lines.push(`---${signature.doc}`);
    if (signature.generic) {
        lines.push(`---@generic ${signature.generic}`);
    }
    for (const [param, type] of signature.params) {
        lines.push(`---@param ${param} ${type}`);
    }
    for (const returned of [signature.returns].flat()) {
        lines.push(`---@return ${returned}`);
    }
    for (const overload of signature.overloads ?? []) {
        lines.push(`---@overload ${overload}`);
    }
    const args = signature.params.map(([param]) => param.replace('?', '')).join(', ');
    lines.push(declaration(owner, member, args), '');
};
for (const name of sorted.filter((n) => !names.get(n))) {
    emit('glm', name, SIGNATURES[name]);
}
for (const [library, members] of [...geometry].sort(([a], [b]) => a.localeCompare(b))) {
    lines.push(...(GEOMETRY_CLASSES[library] ?? [`---Geometry helpers for ${library} shapes.`, `---@class glm.${library}`]), `glm.${library} = {}`, '');
    for (const member of members) {
        emit(`glm.${library}`, member, GEOMETRY_SIGNATURES[`${library}.${member}`]);
    }
}

const target = join(dirname(fileURLToPath(import.meta.url)), '..', 'crates', 'qbx_fivem_data', 'stubs', 'glm.lua');
writeFileSync(target, lines.join('\n'));
const geometryCount = [...geometry.values()].reduce((sum, members) => sum + members.length, 0);
console.log(`wrote ${sorted.length} glm members and ${geometryCount} in ${geometry.size} geometry libraries to ${target}`);
