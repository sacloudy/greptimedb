// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use catalog::CatalogManagerRef;
use client::client_manager::DatanodeClients;
use common_base::Plugins;
use common_function::scalars::aggregate::AggregateFunctionMetaRef;
use common_query::physical_plan::SessionContext;
use common_query::prelude::ScalarUdf;
use datafusion::catalog::catalog::MemoryCatalogList;
use datafusion::dataframe::DataFrame;
use datafusion::error::Result as DfResult;
use datafusion::execution::context::{QueryPlanner, SessionConfig, SessionState};
use datafusion::execution::runtime_env::RuntimeEnv;
use datafusion::physical_optimizer::dist_enforcement::EnforceDistribution;
use datafusion::physical_optimizer::repartition::Repartition;
use datafusion::physical_optimizer::sort_enforcement::EnforceSorting;
use datafusion::physical_optimizer::PhysicalOptimizerRule;
use datafusion::physical_plan::planner::{DefaultPhysicalPlanner, ExtensionPlanner};
use datafusion::physical_plan::{ExecutionPlan, PhysicalPlanner};
use datafusion_expr::LogicalPlan as DfLogicalPlan;
use datafusion_optimizer::analyzer::Analyzer;
use datafusion_optimizer::optimizer::Optimizer;
use partition::manager::PartitionRuleManager;
use promql::extension_plan::PromExtensionPlanner;
use table::table::adapter::DfTableProviderAdapter;
use table::TableRef;

use crate::dist_plan::{DistExtensionPlanner, DistPlannerAnalyzer};
use crate::extension_serializer::ExtensionSerializer;
use crate::optimizer::order_hint::OrderHintRule;
use crate::optimizer::type_conversion::TypeConversionRule;
use crate::query_engine::options::QueryOptions;

/// Query engine global state
// TODO(yingwen): This QueryEngineState still relies on datafusion, maybe we can define a trait for it,
// which allows different implementation use different engine state. The state can also be an associated
// type in QueryEngine trait.
#[derive(Clone)]
pub struct QueryEngineState {
    df_context: SessionContext,
    catalog_manager: CatalogManagerRef,
    aggregate_functions: Arc<RwLock<HashMap<String, AggregateFunctionMetaRef>>>,
    plugins: Arc<Plugins>,
}

impl fmt::Debug for QueryEngineState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("QueryEngineState")
            .field("state", &self.df_context.state())
            .finish()
    }
}

impl QueryEngineState {
    pub fn new(
        catalog_list: CatalogManagerRef,
        with_dist_planner: bool,
        partition_manager: Option<Arc<PartitionRuleManager>>,
        datanode_clients: Option<Arc<DatanodeClients>>,
        plugins: Arc<Plugins>,
    ) -> Self {
        let runtime_env = Arc::new(RuntimeEnv::default());
        let session_config = SessionConfig::new().with_create_default_catalog_and_schema(false);
        // Apply the type conversion rule first.
        let mut analyzer = Analyzer::new();
        if with_dist_planner {
            analyzer.rules.insert(0, Arc::new(DistPlannerAnalyzer));
        }
        analyzer.rules.insert(0, Arc::new(TypeConversionRule));
        let mut optimizer = Optimizer::new();
        optimizer.rules.push(Arc::new(OrderHintRule));

        let mut physical_optimizers = {
            let state = SessionState::with_config_rt(session_config.clone(), runtime_env.clone());
            state.physical_optimizers().to_vec()
        };
        // run the repartition and sort enforcement rules first.
        // And `EnforceSorting` is required to run after `EnforceDistribution`.
        Self::remove_physical_optimize_rule(&mut physical_optimizers, EnforceSorting {}.name());
        Self::remove_physical_optimize_rule(
            &mut physical_optimizers,
            EnforceDistribution {}.name(),
        );
        Self::remove_physical_optimize_rule(&mut physical_optimizers, Repartition {}.name());
        physical_optimizers.insert(0, Arc::new(EnforceSorting {}));
        physical_optimizers.insert(0, Arc::new(EnforceDistribution {}));
        physical_optimizers.insert(0, Arc::new(Repartition {}));

        let session_state = SessionState::with_config_rt_and_catalog_list(
            session_config,
            runtime_env,
            Arc::new(MemoryCatalogList::default()), // pass a dummy catalog list
        )
        .with_serializer_registry(Arc::new(ExtensionSerializer))
        .with_analyzer_rules(analyzer.rules)
        .with_query_planner(Arc::new(DfQueryPlanner::new(
            partition_manager,
            datanode_clients,
        )))
        .with_optimizer_rules(optimizer.rules)
        .with_physical_optimizer_rules(physical_optimizers);

        let df_context = SessionContext::with_state(session_state);

        Self {
            df_context,
            catalog_manager: catalog_list,
            aggregate_functions: Arc::new(RwLock::new(HashMap::new())),
            plugins,
        }
    }

    fn remove_physical_optimize_rule(
        rules: &mut Vec<Arc<dyn PhysicalOptimizerRule + Send + Sync>>,
        name: &str,
    ) {
        let mut index_to_move = None;
        for (i, rule) in rules.iter().enumerate() {
            if rule.name() == name {
                index_to_move = Some(i);
                break;
            }
        }
        if let Some(index) = index_to_move {
            let _ = rules.remove(index);
        }
    }

    /// Register a udf function
    // TODO(dennis): manage UDFs by ourself.
    pub fn register_udf(&self, udf: ScalarUdf) {
        self.df_context.register_udf(udf.into_df_udf());
    }

    pub fn aggregate_function(&self, function_name: &str) -> Option<AggregateFunctionMetaRef> {
        self.aggregate_functions
            .read()
            .unwrap()
            .get(function_name)
            .cloned()
    }

    /// Register an aggregate function.
    ///
    /// # Panics
    /// Will panic if the function with same name is already registered.
    ///
    /// Panicking consideration: currently the aggregated functions are all statically registered,
    /// user cannot define their own aggregate functions on the fly. So we can panic here. If that
    /// invariant is broken in the future, we should return an error instead of panicking.
    pub fn register_aggregate_function(&self, func: AggregateFunctionMetaRef) {
        let name = func.name();
        let x = self
            .aggregate_functions
            .write()
            .unwrap()
            .insert(name.clone(), func);
        assert!(
            x.is_none(),
            "Already registered aggregate function '{name}'"
        );
    }

    #[inline]
    pub fn catalog_manager(&self) -> &CatalogManagerRef {
        &self.catalog_manager
    }

    pub(crate) fn disallow_cross_schema_query(&self) -> bool {
        self.plugins
            .map::<QueryOptions, _, _>(|x| x.disallow_cross_schema_query)
            .unwrap_or(false)
    }

    pub(crate) fn session_state(&self) -> SessionState {
        self.df_context.state()
    }

    /// Create a DataFrame for a table
    pub fn read_table(&self, table: TableRef) -> DfResult<DataFrame> {
        self.df_context
            .read_table(Arc::new(DfTableProviderAdapter::new(table)))
    }
}

struct DfQueryPlanner {
    physical_planner: DefaultPhysicalPlanner,
}

#[async_trait]
impl QueryPlanner for DfQueryPlanner {
    async fn create_physical_plan(
        &self,
        logical_plan: &DfLogicalPlan,
        session_state: &SessionState,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        self.physical_planner
            .create_physical_plan(logical_plan, session_state)
            .await
    }
}

impl DfQueryPlanner {
    fn new(
        partition_manager: Option<Arc<PartitionRuleManager>>,
        datanode_clients: Option<Arc<DatanodeClients>>,
    ) -> Self {
        let mut planners: Vec<Arc<dyn ExtensionPlanner + Send + Sync>> =
            vec![Arc::new(PromExtensionPlanner)];
        if let Some(partition_manager) = partition_manager
         && let Some(datanode_clients) = datanode_clients {
            planners.push(Arc::new(DistExtensionPlanner::new(partition_manager, datanode_clients)));
        }
        Self {
            physical_planner: DefaultPhysicalPlanner::with_extension_planners(planners),
        }
    }
}
