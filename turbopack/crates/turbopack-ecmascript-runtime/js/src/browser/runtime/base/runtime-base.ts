/**
 * This file contains runtime types and functions that are shared between all
 * Turbopack *development* ECMAScript runtimes.
 *
 * It will be appended to the runtime code of each runtime right after the
 * shared runtime utils.
 */

/* eslint-disable @typescript-eslint/no-unused-vars */

/// <reference path="../../../shared/runtime-utils.ts" />

declare var TURBOPACK_WORKER_LOCATION: string;
declare var CHUNK_BASE_PATH: string;

type ModuleFactory = (
  this: Module["exports"],
  context: TurbopackBaseContext
) => undefined;

type DevRuntimeParams = {
  otherChunks: ChunkData[];
  runtimeModuleIds: ModuleId[];
};

type ChunkRegistration = [
  chunkPath: ChunkPath,
  chunkModules: ModuleFactories,
  params: DevRuntimeParams | undefined
];
type ChunkList = {
  path: ChunkPath;
  chunks: ChunkData[];
  source: "entry" | "dynamic";
};

enum SourceType {
  /**
   * The module was instantiated because it was included in an evaluated chunk's
   * runtime.
   */
  Runtime = 0,
  /**
   * The module was instantiated because a parent module imported it.
   */
  Parent = 1,
  /**
   * The module was instantiated because it was included in a chunk's hot module
   * update.
   */
  Update = 2,
}

type SourceInfo =
  | {
      type: SourceType.Runtime;
      chunkPath: ChunkPath;
    }
  | {
      type: SourceType.Parent;
      parentId: ModuleId;
    }
  | {
      type: SourceType.Update;
      parents?: ModuleId[];
    };

interface RuntimeBackend {
  registerChunk: (chunkPath: ChunkPath, params?: DevRuntimeParams) => void;
  loadChunk: (chunkPath: ChunkPath, source: SourceInfo) => Promise<void>;
  reloadChunk?: (chunkPath: ChunkPath) => Promise<void>;
  unloadChunk?: (chunkPath: ChunkPath) => void;

  restart: () => void;
}

const moduleFactories: ModuleFactories = Object.create(null);
const moduleCache: ModuleCache = Object.create(null);
/**
 * Module IDs that are instantiated as part of the runtime of a chunk.
 */
const runtimeModules: Set<ModuleId> = new Set();
/**
 * Map from module ID to the chunks that contain this module.
 *
 * In HMR, we need to keep track of which modules are contained in which so
 * chunks. This is so we don't eagerly dispose of a module when it is removed
 * from chunk A, but still exists in chunk B.
 */
const moduleChunksMap: Map<ModuleId, Set<ChunkPath>> = new Map();
/**
 * Map from a chunk path to all modules it contains.
 */
const chunkModulesMap: Map<ModuleId, Set<ChunkPath>> = new Map();
/**
 * Chunk lists that contain a runtime. When these chunk lists receive an update
 * that can't be reconciled with the current state of the page, we need to
 * reload the runtime entirely.
 */
const runtimeChunkLists: Set<ChunkPath> = new Set();
/**
 * Map from a chunk list to the chunk paths it contains.
 */
const chunkListChunksMap: Map<ChunkPath, Set<ChunkPath>> = new Map();
/**
 * Map from a chunk path to the chunk lists it belongs to.
 */
const chunkChunkListsMap: Map<ChunkPath, Set<ChunkPath>> = new Map();

const availableModules: Map<ModuleId, Promise<any> | true> = new Map();

const availableModuleChunks: Map<ChunkPath, Promise<any> | true> = new Map();

async function loadChunk(
  source: SourceInfo,
  chunkData: ChunkData
): Promise<any> {
  if (typeof chunkData === "string") {
    return loadChunkPath(source, chunkData);
  }

  const includedList = chunkData.included || [];
  const modulesPromises = includedList.map((included) => {
    if (moduleFactories[included]) return true;
    return availableModules.get(included);
  });
  if (modulesPromises.length > 0 && modulesPromises.every((p) => p)) {
    // When all included items are already loaded or loading, we can skip loading ourselves
    return Promise.all(modulesPromises);
  }

  const includedModuleChunksList = chunkData.moduleChunks || [];
  const moduleChunksPromises = includedModuleChunksList
    .map((included) => {
      // TODO(alexkirsz) Do we need this check?
      // if (moduleFactories[included]) return true;
      return availableModuleChunks.get(included);
    })
    .filter((p) => p);

  let promise;
  if (moduleChunksPromises.length > 0) {
    // Some module chunks are already loaded or loading.

    if (moduleChunksPromises.length === includedModuleChunksList.length) {
      // When all included module chunks are already loaded or loading, we can skip loading ourselves
      return Promise.all(moduleChunksPromises);
    }

    const moduleChunksToLoad: Set<ChunkPath> = new Set();
    for (const moduleChunk of includedModuleChunksList) {
      if (!availableModuleChunks.has(moduleChunk)) {
        moduleChunksToLoad.add(moduleChunk);
      }
    }

    for (const moduleChunkToLoad of moduleChunksToLoad) {
      const promise = loadChunkPath(source, moduleChunkToLoad);

      availableModuleChunks.set(moduleChunkToLoad, promise);

      moduleChunksPromises.push(promise);
    }

    promise = Promise.all(moduleChunksPromises);
  } else {
    promise = loadChunkPath(source, chunkData.path);

    // Mark all included module chunks as loading if they are not already loaded or loading.
    for (const includedModuleChunk of includedModuleChunksList) {
      if (!availableModuleChunks.has(includedModuleChunk)) {
        availableModuleChunks.set(includedModuleChunk, promise);
      }
    }
  }

  for (const included of includedList) {
    if (!availableModules.has(included)) {
      // It might be better to race old and new promises, but it's rare that the new promise will be faster than a request started earlier.
      // In production it's even more rare, because the chunk optimization tries to deduplicate modules anyway.
      availableModules.set(included, promise);
    }
  }

  return promise;
}

async function loadChunkPath(
  source: SourceInfo,
  chunkPath: ChunkPath
): Promise<any> {
  try {
    await BACKEND.loadChunk(chunkPath, source);
  } catch (error) {
    let loadReason;
    switch (source.type) {
      case SourceType.Runtime:
        loadReason = `as a runtime dependency of chunk ${source.chunkPath}`;
        break;
      case SourceType.Parent:
        loadReason = `from module ${source.parentId}`;
        break;
      case SourceType.Update:
        loadReason = "from an HMR update";
        break;
      default:
        invariant(source, (source) => `Unknown source type: ${source?.type}`);
    }
    throw new Error(
      `Failed to load chunk ${chunkPath} ${loadReason}${
        error ? `: ${error}` : ""
      }`,
      error
        ? {
            cause: error,
          }
        : undefined
    );
  }
}

/**
 * Returns an absolute url to an asset.
 */
function createResolvePathFromModule(
  resolver: (moduleId: string) => Exports
): (moduleId: string) => string {
  return function resolvePathFromModule(moduleId: string): string {
    const exported = resolver(moduleId);
    return exported?.default ?? exported;
  };
}

function instantiateModule(id: ModuleId, source: SourceInfo): Module {
  const moduleFactory = moduleFactories[id];
  if (typeof moduleFactory !== "function") {
    // This can happen if modules incorrectly handle HMR disposes/updates,
    // e.g. when they keep a `setTimeout` around which still executes old code
    // and contains e.g. a `require("something")` call.
    let instantiationReason;
    switch (source.type) {
      case SourceType.Runtime:
        instantiationReason = `as a runtime entry of chunk ${source.chunkPath}`;
        break;
      case SourceType.Parent:
        instantiationReason = `because it was required from module ${source.parentId}`;
        break;
      case SourceType.Update:
        instantiationReason = "because of an HMR update";
        break;
      default:
        invariant(source, (source) => `Unknown source type: ${source?.type}`);
    }
    throw new Error(
      `Module ${id} was instantiated ${instantiationReason}, but the module factory is not available. It might have been deleted in an HMR update.`
    );
  }

  let parents: ModuleId[];
  switch (source.type) {
    case SourceType.Runtime:
      runtimeModules.add(id);
      parents = [];
      break;
    case SourceType.Parent:
      // No need to add this module as a child of the parent module here, this
      // has already been taken care of in `getOrInstantiateModuleFromParent`.
      parents = [source.parentId];
      break;
    case SourceType.Update:
      parents = source.parents || [];
      break;
    default:
      invariant(source, (source) => `Unknown source type: ${source?.type}`);
  }

  const module: Module = {
    exports: {},
    error: undefined,
    loaded: false,
    id,
    parents,
    children: [],
    namespaceObject: undefined,
  };

  moduleCache[id] = module;

  // NOTE(alexkirsz) This can fail when the module encounters a runtime error.
  try {
    const sourceInfo: SourceInfo = { type: SourceType.Parent, parentId: id };

    const r = commonJsRequire.bind(null, module);
    moduleFactory.call(
      module.exports,
      {
        a: asyncModule.bind(null, module),
        e: module.exports,
        r: commonJsRequire.bind(null, module),
        t: runtimeRequire,
        f: moduleContext,
        i: esmImport.bind(null, module),
        s: esmExport.bind(null, module, module.exports),
        j: dynamicExport.bind(null, module, module.exports),
        v: exportValue.bind(null, module),
        n: exportNamespace.bind(null, module),
        m: module,
        c: moduleCache,
        M: moduleFactories,
        l: loadChunk.bind(null, sourceInfo),
        w: loadWebAssembly.bind(null, sourceInfo),
        u: loadWebAssemblyModule.bind(null, sourceInfo),
        g: globalThis,
        P: resolveAbsolutePath,
        U: relativeURL,
        R: createResolvePathFromModule(r),
        b: getWorkerBlobURL,
        __dirname: typeof module.id === "string" ? module.id.replace(/(^|\/)\/+$/, "") : module.id
      }
    );
  } catch (error) {
    module.error = error as any;
    throw error;
  }

  module.loaded = true;
  if (module.namespaceObject && module.exports !== module.namespaceObject) {
    // in case of a circular dependency: cjs1 -> esm2 -> cjs1
    interopEsm(module.exports, module.namespaceObject);
  }

  return module;
}

/**
 * no-op for browser
 * @param modulePath
 */
function resolveAbsolutePath(modulePath?: string): string {
  return `/ROOT/${modulePath ?? ""}`;
}

function getWorkerBlobURL(chunks: ChunkPath[]): string {
  let bootstrap = `TURBOPACK_WORKER_LOCATION = ${JSON.stringify(location.origin)};importScripts(${chunks.map(c => (`TURBOPACK_WORKER_LOCATION + ${JSON.stringify(getChunkRelativeUrl(c))}`)).join(", ")});`;
  let blob = new Blob([bootstrap], { type: "text/javascript" });
  return URL.createObjectURL(blob);
}

/**
 * Retrieves a module from the cache, or instantiate it if it is not cached.
 */
const getOrInstantiateModuleFromParent: GetOrInstantiateModuleFromParent = (
  id,
  sourceModule
) => {
  const module = moduleCache[id];

  if (sourceModule.children.indexOf(id) === -1) {
    sourceModule.children.push(id);
  }

  if (module) {
    if (module.parents.indexOf(sourceModule.id) === -1) {
      module.parents.push(sourceModule.id);
    }

    return module;
  }

  return instantiateModule(id, {
    type: SourceType.Parent,
    parentId: sourceModule.id,
  });
};

/**
 * Adds a module to a chunk.
 */
function addModuleToChunk(moduleId: ModuleId, chunkPath: ChunkPath) {
  let moduleChunks = moduleChunksMap.get(moduleId);
  if (!moduleChunks) {
    moduleChunks = new Set([chunkPath]);
    moduleChunksMap.set(moduleId, moduleChunks);
  } else {
    moduleChunks.add(chunkPath);
  }

  let chunkModules = chunkModulesMap.get(chunkPath);
  if (!chunkModules) {
    chunkModules = new Set([moduleId]);
    chunkModulesMap.set(chunkPath, chunkModules);
  } else {
    chunkModules.add(moduleId);
  }
}

/**
 * Returns the first chunk that included a module.
 * This is used by the Node.js backend, hence why it's marked as unused in this
 * file.
 */
function getFirstModuleChunk(moduleId: ModuleId) {
  const moduleChunkPaths = moduleChunksMap.get(moduleId);
  if (moduleChunkPaths == null) {
    return null;
  }

  return moduleChunkPaths.values().next().value;
}

/**
 * Removes a module from a chunk.
 * Returns `true` if there are no remaining chunks including this module.
 */
function removeModuleFromChunk(
  moduleId: ModuleId,
  chunkPath: ChunkPath
): boolean {
  const moduleChunks = moduleChunksMap.get(moduleId)!;
  moduleChunks.delete(chunkPath);

  const chunkModules = chunkModulesMap.get(chunkPath)!;
  chunkModules.delete(moduleId);

  const noRemainingModules = chunkModules.size === 0;
  if (noRemainingModules) {
    chunkModulesMap.delete(chunkPath);
  }

  const noRemainingChunks = moduleChunks.size === 0;
  if (noRemainingChunks) {
    moduleChunksMap.delete(moduleId);
  }

  return noRemainingChunks;
}

/**
 * Instantiates a runtime module.
 */
function instantiateRuntimeModule(
  moduleId: ModuleId,
  chunkPath: ChunkPath
): Module {
  return instantiateModule(moduleId, { type: SourceType.Runtime, chunkPath });
}

/**
 * Gets or instantiates a runtime module.
 */
function getOrInstantiateRuntimeModule(
  moduleId: ModuleId,
  chunkPath: ChunkPath
): Module {
  const module = moduleCache[moduleId];
  if (module) {
    if (module.error) {
      throw module.error;
    }
    return module;
  }

  return instantiateModule(moduleId, { type: SourceType.Runtime, chunkPath });
}

/**
 * Returns the URL relative to the origin where a chunk can be fetched from.
 */
function getChunkRelativeUrl(chunkPath: ChunkPath): string {
  return `${CHUNK_BASE_PATH}${chunkPath
    .split("/")
    .map((p) => encodeURIComponent(p))
    .join("/")}`;
}

/**
 * Marks a chunk list as a runtime chunk list. There can be more than one
 * runtime chunk list. For instance, integration tests can have multiple chunk
 * groups loaded at runtime, each with its own chunk list.
 */
function markChunkListAsRuntime(chunkListPath: ChunkPath) {
  runtimeChunkLists.add(chunkListPath);
}

function registerChunk([
  chunkPath,
  chunkModules,
  runtimeParams,
]: ChunkRegistration) {
  for (const [moduleId, moduleFactory] of Object.entries(chunkModules)) {
    if (!moduleFactories[moduleId]) {
      moduleFactories[moduleId] = moduleFactory;
    }
    addModuleToChunk(moduleId, chunkPath);
  }

  return BACKEND.registerChunk(chunkPath, runtimeParams);
}

// const chunkListsToRegister = globalThis.TURBOPACK_CHUNK_LISTS;
// if (Array.isArray(chunkListsToRegister)) {
//   for (const chunkList of chunkListsToRegister) {
//     registerChunkList(globalThis.TURBOPACK_CHUNK_UPDATE_LISTENERS, chunkList);
//   }
// }

// globalThis.TURBOPACK_CHUNK_LISTS = {
//   push: (chunkList) => {
//     registerChunkList(globalThis.TURBOPACK_CHUNK_UPDATE_LISTENERS!, chunkList);
//   },
// } satisfies ChunkListProvider;
