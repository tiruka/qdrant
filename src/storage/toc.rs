use std::sync::{Arc, RwLock};
use collection::collection::Collection;
use std::collections::HashMap;
use crate::operations::collection_ops::{CollectionOps, AliasOperations};
use wal::WalOptions;
use crate::settings::{StorageConfig, Settings, OptimizersConfig};
use tokio::runtime::Runtime;
use tokio::runtime;
use num_cpus;
use std::cmp::max;
use segment::types::SegmentConfig;
use std::path::Path;
use std::fs::{create_dir_all, remove_dir_all};
use crate::operations::errors::ServiceError;
use collection::collection_builder::optimizers_builder::build_optimizers;
use collection::collection_builder::collection_builder::build_collection;


struct TableOfContent {
    collections: Arc<RwLock<HashMap<String, Collection>>>,
    aliases: Arc<RwLock<HashMap<String, String>>>,
    storage_config: StorageConfig,
    optimizers_config: OptimizersConfig,
    search_runtime: Runtime,
    optimization_runtime: Runtime,
}


impl TableOfContent {
    pub fn new(
        collections: HashMap<String, Collection>,
        aliases: HashMap<String, String>,
        settings: &Settings,
    ) -> Self {
        let mut search_threads = settings.performance.max_search_threads;

        if search_threads == 0 {
            let num_cpu = num_cpus::get();
            search_threads = max(1, num_cpu - 1);
        }

        let search_runtime: Runtime = runtime::Builder::new()
            .threaded_scheduler()
            .max_threads(search_threads)
            .build().unwrap();

        let mut optimization_threads = settings.performance.max_optimize_threads;
        if optimization_threads == 0 {
            optimization_threads = 1;
        }

        let optimization_runtime: Runtime = runtime::Builder::new()
            .threaded_scheduler()
            .max_threads(optimization_threads)
            .build().unwrap();


        TableOfContent {
            collections: Arc::new(RwLock::new(collections)),
            aliases: Arc::new(RwLock::new(aliases)),
            storage_config: settings.storage.clone(),
            optimizers_config: settings.optimizers.clone(),
            search_runtime,
            optimization_runtime,
        }
    }

    fn collection_storage_path(&self, collection_name: &str) -> Result<String, ServiceError> {
        let path = Path::new(&self.storage_config.storage_path)
            .join("collections")
            .join(collection_name);

        let path_str = path.to_str()
            .ok_or_else(|| ServiceError::ServiceError {
                description: format!("Invalid collection name {}", collection_name)
            })?;

        create_dir_all(&path)
            .or_else(|err| Err(ServiceError::ServiceError {
                description: format!("Can't create collection directory {} Error: {}", path_str, err)
            }))?;

        Ok(path_str.to_string())
    }

    fn validate_collection_name(&self, collection_name: &str) -> Result<(), ServiceError> {
        if self.collections.read().unwrap().contains_key(collection_name) {
            return Err(ServiceError::BadInput {
                description: format!("Collection {} already exists!", collection_name)
            });
        }

        Ok(())
    }


    pub fn perform_collection_operation(&self, operation: CollectionOps) -> Result<bool, ServiceError> {
        match operation {
            CollectionOps::CreateCollection {
                collection_name,
                dim,
                distance,
                index
            } => {
                self.validate_collection_name(&collection_name)?;

                let wal_options = WalOptions {
                    segment_capacity: self.storage_config.wal_capacity_mb * 1024 * 1024,
                    segment_queue_len: self.storage_config.wal_segments_ahead,
                };

                let collection_path = self.collection_storage_path(&collection_name)?;

                let segment_config = SegmentConfig {
                    vector_size: dim,
                    index: index.unwrap_or(Default::default()),
                    distance,
                    storage_path: collection_path,
                };

                let optimizers = build_optimizers(
                    segment_config.clone(),
                    self.optimizers_config.deleted_threshold,
                    self.optimizers_config.vacuum_min_vector_number,
                    self.optimizers_config.max_segment_number,
                );

                let segment = build_collection(
                    Path::new(&self.storage_config.wal_path),
                    &wal_options,
                    &segment_config,
                    self.search_runtime.handle().clone(),
                    self.optimization_runtime.handle().clone(),
                    optimizers,
                )?;

                let mut write_collections = self.collections.write().unwrap();
                write_collections.insert(collection_name, segment);
                Ok(true)
            }
            CollectionOps::DeleteCollection { collection_name } => {
                let removed = self.collections.write().unwrap().remove(&collection_name).is_some();
                if removed {
                    let path = self.collection_storage_path(&collection_name)?;
                    remove_dir_all(path);
                }
                Ok(removed)
            }
            CollectionOps::ChangeAliases { actions } => {
                let mut write_aliases = self.aliases.write().unwrap();
                for action in actions {
                    match action {
                        AliasOperations::CreateAlias { collection_name, alias_name } => {
                            self.validate_collection_name(&collection_name)?;
                            write_aliases.insert(alias_name, collection_name);
                        }
                        AliasOperations::DeleteAlias { alias_name } => {
                            write_aliases.remove(&alias_name);
                        }
                        AliasOperations::RenameAlias { old_alias_name, new_alias_name } => {
                            if !write_aliases.contains_key(&old_alias_name) {
                                return Err(ServiceError::NotFound { description: format!("Alias {} does not exists!", old_alias_name)})
                            }
                            let collection_name = write_aliases.remove(&old_alias_name).unwrap();
                            write_aliases.insert(new_alias_name, collection_name);
                        }
                    };
                }
                Ok(true)
            }
        }
    }
}