pub mod corners;
mod geometry;
pub mod graph;
mod util;

use corners::Corners;
use geometry::euclidean_distance;
use geometry::fermat_point;
use geometry::overlap;
use geometry::Bounds;
use indexmap::IndexSet;
use itertools::Itertools;
use ordered_float::OrderedFloat;
use petgraph::data::FromElements;
use petgraph::visit::EdgeRef;

use rand::seq::SliceRandom;
use rand::{distributions::Uniform, prelude::Distribution, Rng, SeedableRng};
use util::to_graph;
use util::to_point;

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::SystemTime;

use crate::util::is_improvement_by_factor;

/// a location in 2D
type Point = (f32, f32);

const POPULATION_SIZE: usize = 500;
/// the minimum multiplier to the average terminal distance by which a Steiner
/// point will be moved. In the original paper this value is always used after
/// 1000 generations have passed.
const M_RANGE_MIN: f32 = 0.01;
/// the number of new individuals to create every generation. In the original
/// StOBGA this value is fixed at 166.
const NUMBER_OFFSPRING: usize = POPULATION_SIZE / 3;
/// the smallest probability by which a flip_move_mutation is going to occur.
const P_FLIP_MOVE_MIN: f32 = 0.6;
const P_FLIP_MOVE_MAX: f32 = 0.99;
/// represents an infinitely large value without getting dangerously close to
/// the limits of this datatype.
const INF: f32 = 1e10;
/// a small value, usually utilized to make up for floating point imprecisions.
const EPSILON: f32 = 1e-6;
/// amount of generations the algorithm continues whilst not finding
/// a better individual before ending
const RECESSION_DURATION: usize = 500;

enum BufferSelector {
    ChildBuffer,
    Population
}

/// represents a Steiner Problem instance, consisting of terminals, obstacles
/// and their corners, the centroids obtained through Delaunay triangulation,
/// bounds and the average distance between terminals
struct SteinerProblem {
    /// a list of all the terminals to be connected
    terminals: Vec<Point>,
    /// a list of all the obstacles present on the plane
    obstacles: Vec<Obstacle>,
    /// a list of all the obstacles' corners
    obstacle_corners: Vec<Point>,
    /// a list to store the centroids of the triangles, obtained through
    /// Delaunay triangulation
    centroids: Vec<Point>,
    /// the left, topmost and right, bottommost coordinates framing all
    /// terminals and obstacles in a square
    bounds: Bounds,
    /// the mean distance between terminals
    average_terminal_distance: f32,
}

impl SteinerProblem {
    /// constructor taking a vector of terminals (Points) and a list of
    /// Obstacles as its arguments.
    fn new(terminals: Vec<Point>, obstacles: Vec<Obstacle>) -> Self {
        let mut obstacle_corners = Vec::new();
        for obstacle in &obstacles {
            for point in &obstacle.points {
                obstacle_corners.push(*point);
            }
        }
        let mut centroids = Vec::new();
        let vertices = terminals
            .iter()
            .chain(obstacle_corners.iter())
            .map(|(x, y)| delaunator::Point {
                x: *x as f64,
                y: *y as f64,
            })
            .collect::<Vec<_>>();
        let mut triangles = Vec::new();
        for triple in delaunator::triangulate(&vertices)
            .triangles
            .as_slice()
            .windows(3)
        {
            triangles.push([
                (vertices[triple[0]].x as f32, vertices[triple[0]].y as f32),
                (vertices[triple[1]].x as f32, vertices[triple[1]].y as f32),
                (vertices[triple[2]].x as f32, vertices[triple[2]].y as f32),
            ]);
        }
        for [a, b, c] in triangles {
            centroids.push(geometry::centroid(a, b, c));
        }

        let mut bounds = Bounds::default();
        for point in terminals.iter().chain(obstacle_corners.iter()) {
            if point.0 < bounds.min_x {
                bounds.min_x = point.0
            }
            if point.1 < bounds.min_y {
                bounds.min_y = point.1
            }
            if point.0 > bounds.max_x {
                bounds.max_x = point.0
            }
            if point.1 > bounds.max_y {
                bounds.max_y = point.1
            }
        }
        let mut average_terminal_distance = 0.0;
        {
            let n = terminals.len();
            for i in 0..n {
                for j in 0..n {
                    average_terminal_distance += euclidean_distance(terminals[i], terminals[j]);
                }
            }
            average_terminal_distance /= (n*(n-1)) as f32;
        }

        SteinerProblem {
            terminals,
            obstacles,
            obstacle_corners,
            centroids,
            bounds,
            average_terminal_distance,
        }
    }

    /// a function to check whether a given point is located inside a
    /// solid obstacle
    fn coordinates_in_solid_obstacle(&self, coordinates: Point) -> bool {
        for obstacle in self.obstacles.iter() {
            if obstacle.weight == INF {
                if geometry::point_in_polygon(
                    coordinates.0,
                    coordinates.1,
                    &obstacle.points,
                    &obstacle.bounds,
                ) {
                    return true;
                }
            }
        }
        false
    }
}

/// an extension to the usual Point data structure. This one can be hashed and
/// therefore be stored in a HashSet, IndexSet or IndexMap.
type OPoint = (OrderedFloat<f32>, OrderedFloat<f32>);

/// Chromosomes are one of the two building blocks of Individuals.
/// Being the genotype, they hold the crucial information to build the
/// genotype and evaluate its objective function.
///
/// Genotypes contain all Steiner Points an Individual might have.
/// Steiner Points can be stored as Points with 2D coordinates,
/// or through an index for the list of obstacle corners.
#[derive(Clone)]
struct Chromosome {
    steiner_points: IndexSet<OPoint>,
    included_corners: Corners,
}

impl std::fmt::Debug for Chromosome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string = format!("{:?}", self.included_corners);
        let len = string.len();
        f.write_str(
            format!(
                "Chromosome(steinerPoints={:?}, includedObstacleCornersIndices=set([{}]))",
                self.steiner_points
                    .iter()
                    .map(|p| to_point(*p))
                    .collect::<Vec<Point>>(),
                string.chars().skip(1).take(len - 2).collect::<String>()
            )
            .as_str(),
        )
    }
}

/// Small wrapper around a [
/// petgraph::UnGraph](../petgraph/graph/type.UnGraph.html)
/// data structure to cache its summed edge weights.
#[derive(Clone)]
struct MinimumSpanningTree {
    total_weight: f32,
    graph: petgraph::graph::UnGraph<Point, f32, u32>,
}

/// Together a [Chromosome] and a [SteinerProblem] for an Individual.
/// An Individual represents a potential solution that can be evaluated.
/// Individuals are part of [StOBGA]'s population.
/// Individuals can be mutated and crossed over to create new Individuals
#[derive(Clone)]
struct Individual {
    chromosome: Chromosome,
    minimum_spanning_tree: Option<MinimumSpanningTree>,
}

struct StOBGA<R: Rng> {
    problem: SteinerProblem,
    population: Vec<Individual>,
    random_generator: R,
    current_generation: usize,
    child_buffer: Vec<Individual>,
    function_evaluations: u64,
    edge_db: HashMap<(OPoint, OPoint), f32>,
    start_time: SystemTime,
}

impl<R: Rng> StOBGA<R> {
    fn crossover(&mut self, parent_1_index: usize, parent_2_index: usize) {
        let min_x = self.problem.bounds.min_x;
        let max_x = self.problem.bounds.max_x;
        let random_x_value = self.random_generator.gen_range(min_x..max_x);

        let mut steiner_points_1 = IndexSet::new();
        let mut steiner_points_2 = IndexSet::new();

        let mut obstacle_corners_1 = Corners::new();
        let mut obstacle_corners_2 = Corners::new();

        for point in self.population[parent_1_index]
            .chromosome
            .steiner_points
            .iter()
        {
            if *point.0 < random_x_value {
                steiner_points_1.insert(point.clone());
            } else {
                steiner_points_2.insert(point.clone());
            }
        }
        for point in self.population[parent_2_index]
            .chromosome
            .steiner_points
            .iter()
        {
            if *point.0 > random_x_value {
                steiner_points_1.insert(point.clone());
            } else {
                steiner_points_2.insert(point.clone());
            }
        }

        for index in self.population[parent_1_index]
            .chromosome
            .included_corners
            .iter()
        {
            let point = self.problem.obstacle_corners[index];
            if point.0 < random_x_value {
                obstacle_corners_1.insert(index);
            } else {
                obstacle_corners_2.insert(index);
            }
        }

        for index in self.population[parent_2_index]
            .chromosome
            .included_corners
            .iter()
        {
            let point = self.problem.obstacle_corners[index];
            if point.0 > random_x_value {
                obstacle_corners_1.insert(index);
            } else {
                obstacle_corners_2.insert(index);
            }
        }

        self.child_buffer.push(Individual {
            chromosome: Chromosome {
                steiner_points: steiner_points_1,
                included_corners: obstacle_corners_1,
            },
            minimum_spanning_tree: None,
        });
        self.child_buffer.push(Individual {
            chromosome: Chromosome {
                steiner_points: steiner_points_2,
                included_corners: obstacle_corners_2,
            },
            minimum_spanning_tree: None,
        });
    }

    fn mutate_flip_move(&mut self, index: usize) {
        self.child_buffer[index].mutation_flip_move(
            &self.problem,
            &mut self.random_generator,
            self.current_generation,
        );
        if self.child_buffer[index].minimum_spanning_tree.is_none() {
            self.build_mst(index, BufferSelector::ChildBuffer);
        }
    }

    fn mutate_add_steiner(&mut self, index: usize) {
        if self.child_buffer[index].minimum_spanning_tree.is_none() {
            self.build_mst(index, BufferSelector::ChildBuffer);
        }
        self.child_buffer[index].mutation_add_steiner(&self.problem, &mut self.random_generator);
        if self.child_buffer[index].minimum_spanning_tree.is_none() {
            self.build_mst(index, BufferSelector::ChildBuffer);
        }
    }

    fn mutate_remove_steiner(&mut self, index: usize) {
        if self.child_buffer[index].minimum_spanning_tree.is_none() {
            self.build_mst(index, BufferSelector::ChildBuffer);
        }
        self.child_buffer[index].mutation_remove_steiner(&self.problem, &mut self.random_generator);
        if self.child_buffer[index].minimum_spanning_tree.is_none() {
            self.build_mst(index, BufferSelector::ChildBuffer);
        }
    }

    fn mutate(&mut self, index: usize) {
        let p_flip_move = f32::max(
            P_FLIP_MOVE_MAX * (1.0 - (self.current_generation as f32) / 1000.0),
            P_FLIP_MOVE_MIN,
        );
        if self.random_generator.gen_bool(p_flip_move as f64) {
            self.mutate_flip_move(index);
        } else {
            if self.random_generator.gen_bool(0.5) {
                self.mutate_add_steiner(index);
            } else {
                self.mutate_remove_steiner(index);
            }
        }
    }

    fn finalize(&mut self) {
        self.build_msts();
        let best = &mut self.population[0];
        let mut best_copy = best.clone();
        let mst = best_copy.minimum_spanning_tree.as_ref().unwrap();
        let mut rem_add_list = Vec::new();
        for node in mst.graph.node_indices() {
            let n_edges = mst.graph.edges(node).count();
            if n_edges == 3 {
                let mut all = mst.graph.edges(node);
                let a = all.next().unwrap();
                let b = all.next().unwrap();
                let c = all.next().unwrap();
                rem_add_list.push((
                    node,
                    fermat_point(
                        mst.graph[a.target()],
                        mst.graph[b.target()],
                        mst.graph[c.target()],
                        EPSILON,
                    ),
                ));
            }
        }
        for (index, value) in rem_add_list {
            best_copy.minimum_spanning_tree.as_mut().unwrap().graph[index] = value;
        }
        if best_copy
            .minimum_spanning_tree
            .as_ref()
            .unwrap()
            .total_weight
            < best.minimum_spanning_tree.as_ref().unwrap().total_weight
        {
            self.population[0] = best_copy;
        }
    }

    fn new(
        mut rng: R,
        problem: SteinerProblem,
        population_size: usize,
        t1: usize,
        t2: usize,
        t3: usize,
    ) -> Self {
        let mut population = vec![];
        for _ in 0..t1 {
            population.push(Individual {
                chromosome: Chromosome {
                    steiner_points: problem.centroids.iter().map(|&p| to_graph(p)).collect(),
                    included_corners: Corners::new(),
                },
                minimum_spanning_tree: Option::None,
            });
        }

        let k = problem.obstacle_corners.len();
        let n = problem.terminals.len();
        let min_x = problem.bounds.min_x;
        let max_x = problem.bounds.max_x;
        let min_y = problem.bounds.min_y;
        let max_y = problem.bounds.max_y;
        let x_dist = Uniform::new(min_x, max_x);
        let y_dist = Uniform::new(min_y, max_y);
        let all_corners = (0..k).collect::<Corners>();
        for _ in 0..t2 {
            let mut steiner_points = IndexSet::new();
            let r = rng.gen_range(0..(n + k));
            for _ in 0..r {
                steiner_points.insert(to_graph((rng.sample(x_dist), rng.sample(y_dist))));
            }
            population.push(Individual {
                chromosome: Chromosome {
                    steiner_points: steiner_points,
                    included_corners: all_corners.clone(),
                },
                minimum_spanning_tree: Option::None,
            });
        }

        for _ in 0..t3 {
            let distribution = Uniform::new(0, k + 1);
            let amount = rng.sample(distribution);
            let draws = rand::seq::index::sample(&mut rng, k, amount);
            let mut corners = Corners::new();
            for elem in draws {
                corners.insert(elem);
            }

            population.push(Individual {
                chromosome: Chromosome {
                    steiner_points: IndexSet::new(),
                    included_corners: corners,
                },
                minimum_spanning_tree: Option::None,
            })
        }

        let mut stobga = StOBGA {
            problem,
            population,
            random_generator: rng,
            current_generation: 0,
            child_buffer: Vec::new(),
            edge_db: HashMap::new(),
            function_evaluations: 0,
            start_time: SystemTime::now(),
        };
        stobga.build_msts();
        for _ in 0..(population_size - (t1 + t2 + t3)) {
            let p1 = stobga.tournament_select(5, false);
            let p2 = stobga.tournament_select(5, false);
            stobga.crossover(p1, p2);
            stobga.mutate(stobga.child_buffer.len() - 1);
            stobga.mutate(stobga.child_buffer.len() - 2);
            // stobga.build_mst(stobga.child_buffer.len() - 1, BufferSelector::ChildBuffer);
            // stobga.build_mst(stobga.child_buffer.len() - 2, BufferSelector::ChildBuffer);
            if stobga.population.len() + stobga.child_buffer.len() >= 500 {
                while stobga.population.len() + stobga.child_buffer.len() > 500 {
                    stobga.child_buffer.pop();
                }
                break;
            }
        }
        stobga.population.append(&mut stobga.child_buffer);
        stobga.build_msts();
        assert_eq!(stobga.population.len(), POPULATION_SIZE);
        stobga
    }

    fn instance_to_svg(& self, index : usize) -> String {
        let scaling_factor = 1000.0;
        let move_y = self.problem.bounds.max_y*scaling_factor;
        let instance = &self.population[index];
        let mut result = format!("<svg width='{}px' height='{}px'>", self.problem.bounds.max_x*scaling_factor, self.problem.bounds.max_y*scaling_factor).to_string();
        for obstacle in &self.problem.obstacles {
            let mut svg = format!("<polygon style='fill:{}' points='", {
                if obstacle.weight == INF {
                    "#E86441"
                } else {
                    "#FFDD54"
                }
            }).to_string();
            for corner in &obstacle.points {
                svg = format!("{} {},{}", svg, corner.0*scaling_factor, -corner.1*scaling_factor + move_y);
            }
            svg = format!("{}'/>", svg);
            result = format!("{} {}", result, svg);
        }
        let graph = &instance.minimum_spanning_tree.as_ref().unwrap().graph;
        for edge in graph.edge_references() {
            let from = graph[edge.source()];
            let to = graph[edge.target()];
            result = format!("{}<line x1='{}' y1='{}' x2='{}' y2='{}' style='stroke:black;stroke-width:2px'/>", result, from.0*scaling_factor, -from.1*scaling_factor + move_y, to.0*scaling_factor, -to.1*scaling_factor + move_y);
        }
        for steiner_point in instance.chromosome.steiner_points.iter() {
            result = format!("{} <circle cx='{}' cy='{}' r='10' fill='#59CDF7'/>", result, steiner_point.0*scaling_factor, -steiner_point.1*scaling_factor + move_y);
        }
        for corner in instance.chromosome.included_corners.iter() {
            let steiner_point = self.problem.obstacle_corners[corner];
            result = format!("{} <circle cx='{}' cy='{}' r='10' fill='grey'/>", result, steiner_point.0*scaling_factor, -steiner_point.1*scaling_factor + move_y);
        }
        for terminal in self.problem.terminals.iter() {
            result = format!("{} <circle cx='{}' cy='{}' r='10' fill='black'/>", result, terminal.0*scaling_factor, -terminal.1*scaling_factor + move_y);
        }
        format!("{}</svg>", result)
    }

    fn tournament_select(&mut self, size: usize, to_die: bool) -> usize {
        if to_die {
            return rand::seq::index::sample(
                &mut self.random_generator,
                self.population.len(),
                size,
            )
            .iter()
            .max_by(|i1, i2| {
                let w1 = self.population[*i1]
                    .minimum_spanning_tree
                    .as_ref()
                    .unwrap()
                    .total_weight;
                let w2 = self.population[*i2]
                    .minimum_spanning_tree
                    .as_ref()
                    .unwrap()
                    .total_weight;
                w1.total_cmp(&w2)
            })
            .unwrap();
        } else {
            return rand::seq::index::sample(
                &mut self.random_generator,
                self.population.len(),
                size,
            )
            .iter()
            .min_by(|i1, i2| {
                let w1 = self.population[*i1]
                    .minimum_spanning_tree
                    .as_ref()
                    .unwrap()
                    .total_weight;
                let w2 = self.population[*i2]
                    .minimum_spanning_tree
                    .as_ref()
                    .unwrap()
                    .total_weight;
                w1.total_cmp(&w2)
            })
            .unwrap();
        }
    }

    fn step(&mut self) {
        // println!("population size {}", self.population.len());
        let mut indices_to_recombine = HashSet::new();
        while indices_to_recombine.len() < NUMBER_OFFSPRING {
            let p1 = self.tournament_select(5, false);
            // let p2 = self.tournament_select(5, false);
            indices_to_recombine.insert(p1);
            // println!("{}", indices_to_recombine.len());
        }
        let mut indices_to_recombine = indices_to_recombine.iter().sorted().collect::<Vec<_>>();
        indices_to_recombine.shuffle(&mut self.random_generator);
        let mut pair = Vec::new();
        for &&index in indices_to_recombine.iter() {
            if pair.len() == 0{
                pair.push(index);
            } else if pair.len() == 1{
                self.crossover(pair[0], index);
                pair.clear();
            }
            else {
                unreachable!();
            }
        }
        for i in 0..self.child_buffer.len() {
            self.mutate(i);
        }
        let to_die = NUMBER_OFFSPRING;
        for _ in 0..to_die {
            let index = self.tournament_select(5, true);
            self.population.remove(index);
        }
        assert_eq!(self.child_buffer.len(), 166);
        self.population.append(&mut self.child_buffer);
        self.build_msts();
        self.population.sort_unstable_by(|i1, i2| {
            i1.minimum_spanning_tree
                .as_ref()
                .unwrap()
                .total_weight
                .total_cmp(&i2.minimum_spanning_tree.as_ref().unwrap().total_weight)
        });
        self.current_generation += 1;
        assert_eq!(self.population.len(), POPULATION_SIZE);
        assert_eq!(self.child_buffer.len(), 0);
        // println!("{}", "leavin step now");
    }

    fn compute_distance(&self, from: OPoint, to: OPoint) -> f32 {
        let p1 = to_point(from);
        let p2 = to_point(to);
        let mut length = geometry::euclidean_distance(p1, p2);
        let line_bounds = Bounds {
            min_x: p1.0.min(p2.0),
            min_y: p1.1.min(p2.1),
            max_x: p1.0.max(p2.0),
            max_y: p1.1.max(p2.1),
        };
        for obstacle in &self.problem.obstacles {
            let bounds = &obstacle.bounds;
            if overlap(
                line_bounds.min_x,
                line_bounds.min_y,
                line_bounds.max_x,
                line_bounds.max_y,
                bounds.min_x,
                bounds.min_y,
                bounds.max_x,
                bounds.max_y,
            ) {
                let intersection_len = geometry::intersection_length(
                    *from.0,
                    *from.1,
                    *to.0,
                    *to.1,
                    &obstacle.points,
                    &obstacle.bounds,
                );
                if intersection_len > 0.0 {
                    if obstacle.weight == INF {
                        length = INF;
                        break;
                    } else {
                        length -= intersection_len;
                        length += intersection_len * obstacle.weight;
                    }
                }
            }
        }
        length
    }

    fn build_mst(&mut self, index: usize, buffer : BufferSelector) {
        let mut graph = petgraph::graph::UnGraph::new_undirected();
        let individual = match buffer {
            BufferSelector::ChildBuffer => &self.child_buffer[index],
            BufferSelector::Population => &self.population[index],
        };
        let source_vertices = individual
            .chromosome
            .steiner_points
            .iter()
            .map(|&p| p)
            .chain(
                individual
                    .chromosome
                    .included_corners
                    .iter()
                    .map(|c| util::to_graph(self.problem.obstacle_corners[c])),
            )
            .chain(self.problem.terminals.iter().map(|p| to_graph(*p)));
        // let source_vertices = source_vertices.collect_vec();
        for vertex in source_vertices.clone() {
            graph.add_node(to_point(vertex));
        }
        for pair in source_vertices.enumerate().combinations(2) {
            let (i1, t1) = pair[0];
            let (i2, t2) = pair[1];
            // let length = self.get_distance(t1, t2);
            let length = if let Some(&x) = self.edge_db.get(&(t1, t2)) {
                x
            } else if let Some(&x) = self.edge_db.get(&(t2, t1)) {
                x
            } else {
                let d = self.compute_distance(t1, t2);
                self.edge_db.insert((t1, t2), d);
                d
            };
            graph.add_edge(
                petgraph::graph::NodeIndex::new(i1),
                petgraph::graph::NodeIndex::new(i2),
                length,
            );
        }

        let mst = petgraph::graph::UnGraph::<_, _>::from_elements(
            petgraph::algo::min_spanning_tree(&graph),
        );
        let total_distance = mst.edge_weights().sum::<f32>();
        let mst = MinimumSpanningTree {
            total_weight: total_distance,
            graph: mst,
        };
        match buffer {
            BufferSelector::ChildBuffer => self.child_buffer[index].minimum_spanning_tree = Some(mst),
            BufferSelector::Population => self.population[index].minimum_spanning_tree = Some(mst),
        }
        self.function_evaluations += 1;
    }

    fn build_msts(&mut self) {
        for index in 0..self.population.len() {
            if self.population[index].minimum_spanning_tree.is_none() {
                self.build_mst(index, BufferSelector::Population);
            }
        }
    }
}

impl Individual {
    fn mutation_remove_steiner<R: Rng>(&mut self, problem: &SteinerProblem, rng: &mut R) {
        let mut candidate_steiner_points = Vec::new();

        let graph = &self.minimum_spanning_tree.as_ref().unwrap().graph;
        for steiner_point in self.chromosome.steiner_points.iter() {
            let id = graph
                .node_indices()
                .find(|id| graph[*id].0 == *steiner_point.0 && graph[*id].1 == *steiner_point.1)
                .unwrap();
            let edges = graph.edges(id);
            if edges.count() <= 2 {
                candidate_steiner_points.push(*steiner_point);
            }
        }
        let mut candidate_corners = Vec::new();
        for index_corner in self.chromosome.included_corners.iter() {
            let steiner_point = problem.obstacle_corners[index_corner];
            let id = graph
                .node_indices()
                .find(|id| graph[*id].0 == steiner_point.0 && graph[*id].1 == steiner_point.1)
                .unwrap();
            let edges = graph.edges(id);
            if edges.count() <= 2 {
                candidate_corners.push(index_corner.clone());
            }
        }
        match (candidate_steiner_points.len(), candidate_corners.len()) {
            (0, 0) => {}
            (0, n) => {
                self.chromosome
                    .included_corners
                    .remove(&candidate_corners[if n > 1 { rng.gen_range(0..n) } else { 0 }]);
            }
            (n, 0) => {
                self.chromosome
                    .steiner_points
                    .remove(&candidate_steiner_points[if n > 1 { rng.gen_range(0..n) } else { 0 }]);
            }
            (n, m) => {
                if rng.gen_bool((n as f32 / m as f32).clamp(0.0, 1.0) as f64) {
                    self.chromosome.steiner_points.remove(
                        &candidate_steiner_points[if n > 1 { rng.gen_range(0..n) } else { 0 }],
                    );
                } else {
                    self.chromosome
                        .included_corners
                        .remove(&candidate_corners[if m > 1 { rng.gen_range(0..m) } else { 0 }]);
                }
            }
        }
        self.minimum_spanning_tree = None;
    }

    fn mutation_add_steiner<R: Rng>(&mut self, problem: &SteinerProblem, rng: &mut R) {
        let mut candidates = Vec::new();
        let graph = &self.minimum_spanning_tree.as_ref().unwrap().graph;
        for i1 in graph.node_indices() {
            let connections = graph.edges(i1);
            let c1 = graph[i1];
            let v1 = nalgebra::Vector2::new(c1.0, c1.1);
            for edge in connections.combinations(2) {
                let i2 = edge[0].target();
                let i3 = edge[1].target();
                let c2 = graph[i2];
                let c3 = graph[i3];
                let v2 = nalgebra::Vector2::new(c2.0, c2.1);
                let v3 = nalgebra::Vector2::new(c3.0, c3.1);
                let v12 = v2 - v1;
                let v13 = v3 - v1;
                let dot = v12.dot(&v13);
                let den = v12.norm() * v13.norm();
                let angle = (dot / den).acos();
                if angle < geometry::RADIANS_120_DEGREE {
                    candidates.push((i1, i2, i3));
                }
            }
        }
        if candidates.len() == 0 {
            // add random steiner point
            let min_x = problem.bounds.min_x;
            let max_x = problem.bounds.max_x;
            let min_y = problem.bounds.min_y;
            let max_y = problem.bounds.max_y;
            let mut new_steiner = (rng.gen_range(min_x..max_x), rng.gen_range(min_y..max_y));
            while problem.coordinates_in_solid_obstacle(new_steiner) {
                new_steiner = (rng.gen_range(min_x..max_x), rng.gen_range(min_y..max_y));
            }
            self.chromosome.steiner_points.insert(to_graph(new_steiner));
        } else {
            let random_triple = candidates[if candidates.len() > 1 {
                rng.gen_range(0..candidates.len())
            } else {
                0
            }];
            let p1 = graph[random_triple.0];
            let p2 = graph[random_triple.1];
            let p3 = graph[random_triple.2];
            let p4 = geometry::fermat_point(p1, p2, p3, EPSILON);
            if !problem.coordinates_in_solid_obstacle(p4) {
                if match self.chromosome.steiner_points.iter().map(|&s| OrderedFloat::from(euclidean_distance(to_point(s), p4))).min() {
                    Some(OrderedFloat(x)) => x > 1e-2,
                    None => true,
                } {
                    self.chromosome.steiner_points.insert(to_graph(p4));
                }
            }
        }
        self.minimum_spanning_tree = None;
    }

    fn mutation_flip_move<R: Rng>(
        &mut self,
        problem: &SteinerProblem,
        rng: &mut R,
        generation: usize,
    ) {
        let s = self.chromosome.steiner_points.len();
        let k = problem.obstacle_corners.len();
        let p_gene = if s + k == 0 {
            1.0
        } else {
            1.0 / ((s + k) as f32)
        };
        let m_range = problem.average_terminal_distance
            * f32::max(1.0 - (generation as f32) / 1000.0, M_RANGE_MIN);
        let mut to_remove = Vec::new();
        let mut to_add = Vec::new();
        for &steiner_point in self.chromosome.steiner_points.iter() {
            if rng.gen_bool(p_gene as f64) {
                let x_sign = if rng.gen_bool(0.5) { 1.0 } else { -1.0 };
                let y_sign = if rng.gen_bool(0.5) { 1.0 } else { -1.0 };

                to_remove.push(steiner_point);
                if m_range > M_RANGE_MIN {
                    let dist = Uniform::new(M_RANGE_MIN, m_range);
                    to_add.push((
                        OrderedFloat(*steiner_point.0 + dist.sample(rng) * x_sign),
                        OrderedFloat(*steiner_point.1 + dist.sample(rng) * y_sign),
                    ));
                } else {
                    to_add.push((
                        OrderedFloat(*steiner_point.0 + M_RANGE_MIN * x_sign),
                        OrderedFloat(*steiner_point.1 + M_RANGE_MIN * y_sign),
                    ));
                }
            }
        }
        for point in to_remove {
            self.chromosome.steiner_points.remove(&point);
        }
        for point in to_add {
            self.chromosome.steiner_points.insert(point);
        }
        for i in 0..k {
            if rng.gen_bool(p_gene as f64) {
                if self.chromosome.included_corners.contains(&i) {
                    self.chromosome.included_corners.remove(&i);
                } else {
                    self.chromosome.included_corners.insert(i);
                }
            }
        }
        self.minimum_spanning_tree = None
    }
}

#[derive(Clone)]
struct Obstacle {
    weight: f32,
    bounds: Bounds,
    points: Vec<Point>,
}

impl std::fmt::Debug for Obstacle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Obstacle")
            .field("weight", &self.weight)
            .field("bounds", &self.bounds)
            .field("points", &self.points)
            .finish()
    }
}

impl Obstacle {
    fn new(weight: f32, points: Vec<Point>) -> Self {
        Self {
            weight,
            points,
            bounds: Bounds::default(),
        }
    }

    pub(crate) fn compute_bounds(mut self) -> Obstacle {
        let mut bounds = Bounds::default();
        for point in &self.points {
            if point.0 < bounds.min_x {
                bounds.min_x = point.0
            }
            if point.1 < bounds.min_y {
                bounds.min_y = point.1
            }
            if point.0 > bounds.max_x {
                bounds.max_x = point.0
            }
            if point.1 > bounds.max_y {
                bounds.max_y = point.1
            }
        }
        self.bounds = bounds;
        self
    }
}

fn main() {
    std::env::set_var("RUST_BACKTRACE", "full");
    let mut terminals = Vec::new();
    for line in std::fs::read_to_string(
        std::env::args()
            .nth(1)
            .expect("please specify terminal file"),
    )
    .unwrap()
    .lines()
    .skip(1)
    {
        let coords = line
            .split(",")
            .map(|c| c.parse().unwrap())
            .collect::<Vec<_>>();
        terminals.push((coords[0], coords[1]));
    }

    let mut obstacles = Vec::new();
    {
        let mut current_obstacle = Obstacle::new(0.0, vec![]);
        for line in std::fs::read_to_string(
            std::env::args()
                .nth(2)
                .expect("please specify obstacle file"),
        )
        .unwrap()
        .lines()
        {
            if line == "" || line == "," {
                obstacles.push(current_obstacle.compute_bounds());
                current_obstacle = Obstacle::new(0.0, vec![]);
            } else if line.to_lowercase().starts_with("max") {
                current_obstacle.weight = INF
            } else {
                let fields = line.split(",").collect::<Vec<_>>();
                if fields.get(1) == Some(&"") || fields.len() < 2 {
                    current_obstacle.weight = fields[0].parse().unwrap();
                } else {
                    current_obstacle
                        .points
                        .push((fields[0].parse().unwrap(), fields[1].parse().unwrap()));
                }
            }
        }
        obstacles.push(current_obstacle.compute_bounds());
    }

    let seed = match std::env::args().nth(3) {
        Some(a) => a.parse().expect("could not parse seed"),
        None => 0,
    };

    let rng = rand_pcg::Pcg32::seed_from_u64(seed);
    let problem = SteinerProblem::new(terminals.clone(), obstacles.clone());
    let mut stobga = StOBGA::new(rng, problem, POPULATION_SIZE, 1, 50, 50);

    println!(
        "generation§population average§best§chromosome§function evaluations§runtime in seconds§svg§seed={}",
        seed
    );
    stobga.build_msts();
    #[derive(PartialEq)]
    enum LoopState {
        Running,
        LastGeneration,
    }
    struct LoopData {
        state: LoopState,
        streak_length: usize,
        previous_best_weight: f32,
    }
    let mut loop_data = LoopData {
        state: LoopState::Running,
        previous_best_weight: INF,
        streak_length: 0,
    };
    loop {
        stobga.step();
        if loop_data.state == LoopState::LastGeneration {
            stobga.finalize();
        }
        let best = 0;
        let best_weight = stobga.population[best]
            .minimum_spanning_tree
            .as_ref()
            .unwrap()
            .total_weight;
        if is_improvement_by_factor(loop_data.previous_best_weight, best_weight, 0.01 / 100.0)
            || loop_data.state == LoopState::LastGeneration
        {
            loop_data.previous_best_weight = best_weight;
            loop_data.streak_length = 0;
            println!(
                "{}§{}§{}§{:?}§{}§{}§{}",
                stobga.current_generation,
                {
                    util::average_from_iterator(stobga.population.iter().map(|individual| {
                        individual
                            .minimum_spanning_tree
                            .as_ref()
                            .unwrap()
                            .total_weight
                    }))
                },
                {
                    stobga.population[best]
                        .minimum_spanning_tree
                        .as_ref()
                        .unwrap()
                        .total_weight
                },
                stobga.population[best].chromosome,
                stobga.function_evaluations,
                match SystemTime::now().duration_since(stobga.start_time) {
                    Ok(s) => format!("{}", s.as_secs_f32()),
                    Err(_) => format!("NA"),
                },
                stobga.instance_to_svg(0)
            );
        } else {
            loop_data.streak_length += 1
        }
        if loop_data.state == LoopState::LastGeneration {
            break;
        }
        if loop_data.streak_length == RECESSION_DURATION {
            loop_data.state = LoopState::LastGeneration;
        }
    }
}

#[cfg(test)]
mod test {
    use std::{collections::HashSet, time::Instant, fmt::Binary};

    use crate::{
        *, geometry::{intersection_length, middle, point_in_polygon, segment_polygon_intersection}, graph::Graph,
    };
    use itertools::Itertools;
    use petgraph::{data::FromElements, prelude::UnGraph};
    use rand::{Rng, SeedableRng};

    #[test]
    fn test_geometry() {
        assert_eq!(
            crate::geometry::point_in_polygon(
                0.0,
                0.0,
                &[(-1.0, -1.0), (1.0, 1.0), (0.0, 2.0)],
                &geometry::Bounds {
                    min_x: -1.0,
                    max_x: 1.0,
                    min_y: -1.0,
                    max_y: 2.0
                }
            ),
            false
        )
    }

    #[test]
    fn test_geometry2() {
        assert_eq!(
            crate::geometry::segment_polygon_intersection(
                0.0,
                0.0,
                2.0,
                0.0,
                &[(1.0, 0.0), (1.0, -1.0), (-1.0, -1.0)],
                true
            ),
            vec![(1.0, 0.0)]
        );
        assert_eq!(
            crate::geometry::intersection_length(
                0.0,
                0.0,
                2.0,
                0.0,
                &[(1.0, 0.0), (1.0, -1.0), (-1.0, -1.0)],
                &geometry::Bounds {
                    min_x: -1.0,
                    max_x: 1.0,
                    min_y: -1.0,
                    max_y: 0.0
                }
            ),
            0.0
        );
    }

    // #[test]
    // fn test_geometry3() {
    //     assert_eq!(
    //         crate::geometry::segment_polygon_intersection(
    //             0.0,
    //             0.0,
    //             1.0,
    //             1.0,
    //             &[(0.0, 0.0), (1.0, 1.0), (1.0, -1.0)],
    //             true
    //         ),
    //         Vec::new()
    //     )
    // }

    #[test]
    fn test_geometry4() {
        assert_eq!(
            crate::geometry::intersection_length(
                3.0,
                1.0,
                4.0,
                5.0,
                &[(0.0, 0.0), (3.0, 1.0), (4.0, 5.0)],
                &geometry::Bounds {
                    min_x: 0.0,
                    max_x: 4.0,
                    min_y: 0.0,
                    max_y: 5.0
                }
            ),
            0.0
        )
    }

    #[test]
    fn instance_five_issue() {
        // x = 0.3
        let steiner_points = [
            (0.39435774, 0.36414573), 
            (0.478367, 0.45599815), 
            (0.48510268, 0.82256573), 
            (0.5242697, 0.7148127), 
            
            (0.09365932, 0.16696312),
            // (0.09824701, 0.16467005), 
            (0.10451312, 0.3484062), 
            (0.2153477, 0.84840983), 
            // (0.545881, 0.718454), 
            ].iter().map(|&a|to_graph(a)).collect::<IndexSet<_>>();
        let terminals = vec![
            (0.644,0.242),
            (0.24,0.386),
            (0.048,0.39),
            (0.152,0.15000000000000002),
            (0.654,0.698),
            (0.526,0.87),
            (0.156,0.85),
            (0.43,0.5900000000000001),
            (0.91,0.72),
            (0.88,0.634),
            (0.728,0.406),
        ];
        let mut obstacles = vec![
        Obstacle::new(5.0, vec![
            (0.098,0.9),
            (0.21,0.902),
            (0.204,0.488),
            (0.094,0.488),
        ]).compute_bounds(),
        Obstacle::new(5.0, vec![
            (0.602,0.81),
            (0.578,0.6),
            (0.766,0.466),
            (0.912,0.704),
            (0.72,0.622),
            (0.718,0.834)]).compute_bounds(),
        Obstacle::new(5.0,vec![
            (0.45,0.206),
            (0.512,0.414),
            (0.614,0.408),
            (0.732,0.39),
            (0.84,0.398),
            (0.85,0.2),
            (0.644,0.274)]).compute_bounds(),
        Obstacle::new(5.0,vec![
            (0.1,0.304),
            (0.344,0.112),
            (0.096,0.098),
        ]).compute_bounds()
        ];
        let obstacle = Obstacle::new(5.0, vec![
            (0.602,0.81),
            (0.578,0.6),
            (0.766,0.466),
            (0.912,0.704),
            (0.72,0.622),
            (0.718,0.834)]).compute_bounds();
        let rng = rand_pcg::Pcg32::seed_from_u64(2);
        let included_corners = [
            7, 
            10, 
            17
            ].into_iter().collect();
        let instance = SteinerProblem::new(terminals, obstacles);
        let chromosome = Chromosome {
            steiner_points,
            included_corners,
        };
        
        assert!(geometry::point_in_polygon(0.721041977,0.599999964, &obstacle.points, &obstacle.bounds));
        assert!(!geometry::point_in_polygon(0.7965147, 0.48967615, &obstacle.points, &obstacle.bounds));
        assert!(geometry::point_in_polygon(0.622285664, 0.703999758, &obstacle.points, &obstacle.bounds));
        assert!(!geometry::point_in_polygon(0.545881, 0.718454, &obstacle.points, &obstacle.bounds));
        assert!(geometry::intersection_length(0.654, 0.698, 0.545881, 0.718454, &obstacle.points, &obstacle.bounds) > 0.0);
        assert!(geometry::intersection_length(0.545881, 0.718454,0.654, 0.698, &obstacle.points, &obstacle.bounds) > 0.0);
        assert!(geometry::intersection_length(0.7965147, 0.48967615,0.654, 0.698, &obstacle.points, &obstacle.bounds) > 0.0);
        let mut stobga = StOBGA::new(rng,instance, 500, 0, 500, 0);
        
        stobga.child_buffer = vec![Individual{chromosome, minimum_spanning_tree:None}];
        stobga.build_mst(0, BufferSelector::ChildBuffer);
        stobga.population[0] = stobga.child_buffer[0].clone();
        println!("{}\n\n", stobga.instance_to_svg(0));
        stobga.mutate_remove_steiner(0);
        stobga.population[0] = stobga.child_buffer[0].clone();
        stobga.build_mst(0, BufferSelector::ChildBuffer);
        println!("{}\n\n", stobga.instance_to_svg(0));
        // println!("{}",stobga.population[0].minimum_spanning_tree.as_ref().unwrap().total_weight);
        // println!("{}",stobga.instance_to_svg(0));
    }

    #[test]
    fn test_geometry6() {
        let middle = middle(3.0, 1.0, 4.0, 5.0);
        assert!(!point_in_polygon(
            middle.0,
            middle.1,
            &[(0.0, 0.0), (3.0, 1.0), (4.0, 5.0)],
            &geometry::Bounds {
                min_x: 0.0,
                max_x: 4.0,
                min_y: 0.0,
                max_y: 5.0
            }
        ))
    }

    #[test]
    fn test_geometry7() {
        let middle = middle(0.0, 0.0, 4.0, 5.0);
        assert!(!point_in_polygon(
            middle.0,
            middle.1,
            &[(0.0, 0.0), (3.0, 1.0), (4.0, 5.0)],
            &geometry::Bounds {
                min_x: 0.0,
                max_x: 4.0,
                min_y: 0.0,
                max_y: 5.0
            }
        ))
    }

    #[test]
    fn test_geometry8() {
        let middle = middle(0.0, 0.0, 3.0, 1.0);
        assert!(!point_in_polygon(
            middle.0,
            middle.1,
            &[(0.0, 0.0), (3.0, 1.0), (4.0, 5.0)],
            &geometry::Bounds {
                min_x: 0.0,
                max_x: 4.0,
                min_y: 0.0,
                max_y: 5.0
            }
        ))
    }

    #[test]
    fn test_geometry9() {
        assert_eq!(
            crate::geometry::intersection_length(
                0.0,
                1.0,
                1.0,
                1.0,
                &[(0.0, 0.0), (1.0, 0.0), (0.5, -1.0)],
                &geometry::Bounds {
                    min_x: 0.0,
                    max_x: 1.0,
                    min_y: -1.0,
                    max_y: 0.0
                }
            ),
            0.0
        )
    }

    #[test]
    fn test_geometry10() {
        assert!(
            crate::geometry::intersection_length(
                0.845641974,
                0.904959172,
                0.753467217,
                0.42431886,
                &[
                    (0.796, 0.898),
                    (0.804, 0.784),
                    (0.906, 0.792),
                    (0.908, 0.886),
                ],
                &geometry::Bounds {
                    min_x: 0.0,
                    max_x: 1.0,
                    min_y: 0.0,
                    max_y: 1.0
                }
            ) > 0.0
        )
    }

    #[test]
    fn test_geometry11() {
        println!(
            "{}",
            crate::geometry::intersection_length(
                0.936640447,
                0.706594727,
                0.753467217,
                0.42431886,
                &[
                    (0.784, 0.522),
                    (0.798, 0.44799999999999995),
                    (0.906, 0.45199999999999996),
                    (0.9, 0.534),
                ],
                &geometry::Bounds {
                    min_x: 0.0,
                    max_x: 1.0,
                    min_y: 0.0,
                    max_y: 1.0
                }
            )
        );
        assert!(
            crate::geometry::intersection_length(
                0.936640447,
                0.706594727,
                0.753467217,
                0.42431886,
                &[
                    (0.784, 0.522),
                    (0.798, 0.44799999999999995),
                    (0.906, 0.45199999999999996),
                    (0.9, 0.534),
                ],
                &geometry::Bounds {
                    min_x: 0.0,
                    max_x: 1.0,
                    min_y: 0.0,
                    max_y: 1.0
                }
            ) > 0.0
        )
    }

    #[test]
    fn using_petgraph() {
        let mut graph = petgraph::Graph::new_undirected();
        let i1 = graph.add_node((1.0, 1.0));
        let i2 = graph.add_node((2.0, 2.0));
        graph.add_edge(i1, i2, 1.0);
        let g2 = UnGraph::<_, _>::from_elements(petgraph::algo::min_spanning_tree(&graph));
        assert!(g2.edge_weights().sum::<f32>() == 1.0)
    }

    #[test]
    fn seeding_actually_makes_rand_reproducable() {
        let mut rng = rand_pcg::Pcg32::seed_from_u64(0);
        assert_eq!(rng.gen::<u64>(), 18195738587432868099);
        let mut rng1 = rand_pcg::Pcg32::seed_from_u64(0);
        assert_eq!(rng1.gen::<u64>(), 18195738587432868099);
    }

    #[test]
    fn hashing_edges() {
        let e1 = graph::Edge {
            start: util::to_graph((0.0, 0.0)),
            end: util::to_graph((1.0, 1.0)),
        };
        let e2 = graph::Edge {
            end: util::to_graph((0.0, 0.0)),
            start: util::to_graph((1.0, 1.0)),
        };
        let mut set = HashSet::new();
        set.insert(e1);
        set.insert(e2);
        assert!(set.len() == 1);
    }

    #[test]
    fn making_a_graph() {
        let mut graph = graph::Graph::new();
        graph.add_edge_from_points((0.0, 0.0), (1.0, 1.0), 1.0);
        graph.add_edge_from_points((2.0, 0.0), (1.0, 1.0), 1.0);
        graph.add_edge_from_points((0.0, 0.0), (1.0, 0.0), 1.0);
        println!("{:?}", graph.edges_connected_to_point((1.0, 1.0)));
    }

    #[test]
    fn trivial_mst() {
        let mut graph = Graph::new();
        graph.add_edge_from_points((0.0, 0.0), (0.0, 1.0), 1.0);
        graph.add_edge_from_points((1.0, 1.0), (0.0, 1.0), 1.0);
        let mst = graph.minimum_spanning_tree();
        assert_eq!(mst.nodes.len(), 3);
        assert_eq!(mst.edges.len(), 2);
    }

    #[test]
    fn advanced_mst() {
        let mut graph = Graph::new();
        graph.add_edge_from_points((0.0, 0.0), (0.0, 1.0), 1.0);
        graph.add_edge_from_points((0.0, 0.0), (1.0, 1.0), 2.0);
        graph.add_edge_from_points((0.0, 0.0), (1.0, 0.0), 3.0);
        graph.add_edge_from_points((1.0, 1.0), (0.0, 1.0), 4.0);
        graph.add_edge_from_points((1.0, 1.0), (1.0, 0.0), 5.0);
        graph.add_edge_from_points((1.0, 0.0), (0.0, 1.0), 6.0);
        let mst = graph.minimum_spanning_tree();
        assert_eq!(mst.nodes.len(), 4);
        assert_eq!(mst.edges.len(), 3);
        println!("{:?}", mst);
        assert_eq!(mst.edges.values().sum::<f32>(), 6.0);
    }

    // #[test]
    // fn build_binary_corners() {
    //     let mut corners = crate::corners::BinaryCorners::new();
    //     corners.insert(3);
    //     corners.insert(4);
    //     corners.insert(9);
    //     assert_eq!(corners.iter().collect_vec(), vec![3, 4, 9])
    // }

    #[test]
    // fn testing_binary_corners() {
    //     let mut corners = (0..3).collect::<BinaryCorners>();
    //     assert_eq!(corners.included, 7);
    //     corners.remove(&0);
    //     corners.remove(&1);
    //     assert_eq!(corners.included, 4);
    // }

    #[test]
    fn problematic_intersection() {
        let obstacle = Obstacle {
            weight: 4.0,
            bounds: Bounds::default(),
            points: vec![
                (0.116, 0.39),
                (0.096, 0.29),
                (0.084, 0.206),
                (0.104, 0.048),
                (0.31, 0.018),
                (0.542, 0.072),
                (0.5, 0.192),
                (0.338, 0.144),
                (0.256, 0.13),
                (0.208, 0.158),
                (0.208, 0.27),
            ],
        }
        .compute_bounds();
        let start = (0.182, 0.126);
        let end = (0.31, 0.018);
        let distance = intersection_length(
            start.0,
            start.1,
            end.0,
            end.1,
            &obstacle.points,
            &obstacle.bounds,
        );
        assert_eq!(distance, euclidean_distance(start, end));
    }

    #[test]
    fn problematic_lengths() {
        let obstacle1 = Obstacle {
            weight: INF,
            bounds: Bounds::default(),
            points: vec![
                (0.83, 1.33),
                (2.7, 1.19),
                (0.91, 0.36),
                (8.16, 1.31),
                (6.43, 3.06),
            ],
        }
        .compute_bounds();

        let obstacle2 = Obstacle {
            weight: INF,
            bounds: Bounds::default(),
            points: vec![(0.56, 1.27), (2.16, 1.09), (0.56, 0.33), (1.14, 0.88)],
        }
        .compute_bounds();

        let obstacle3 = Obstacle {
            weight: INF,
            bounds: Bounds::default(),
            points: vec![(0.19, 1.21), (0.82, 0.86), (0.18, 0.32)],
        }
        .compute_bounds();

        let steiner1 = (0.56, 0.33);
        let steiner2 = (0.82, 0.86);
        let steiner3 = (0.56, 1.27);

        let terminal1 = (2.89, 0.25);
        let terminal2 = (2.43, 2.08);

        let d1 = euclidean_distance(steiner1, steiner2);
        let d2 = euclidean_distance(steiner2, steiner3);
        let d3 = euclidean_distance(terminal1, steiner1);
        let d4 = euclidean_distance(terminal2, steiner2);

        let convenience = |v1: (f32, f32), v2: (f32, f32), p: Obstacle| {
            geometry::intersection_length(v1.0, v1.1, v2.0, v2.1, &p.points, &p.bounds)
        };
        assert_eq!(convenience(steiner1, steiner2, obstacle1), 0.0);
        assert_eq!(convenience(steiner1, steiner2, obstacle2), 0.0);
        assert_eq!(convenience(steiner1, steiner2, obstacle3), 0.0);
        // assert_eq!(geometry::intersection_length(steiner1.0, steiner1.1, steiner2.0, steiner2.1, &obstacle3.points, &obstacle3.bounds), 0.0);
        // assert_eq!(d1+d2+d3+d4,0.0);
    }

    #[test]
    fn wrapping_an_obstacle() {
        let obstacle = Obstacle {
            points: 
            vec![
                (0.168,0.63),
                (0.168,0.606),
                (0.188,0.5840000000000001),
                (0.226,0.5920000000000001),
                (0.336,0.614),
                (0.392,0.766),
                (0.32,0.758),
                (0.244,0.69),
            ],
            weight: 9999999.0,
            bounds: Bounds::default(),
        }.compute_bounds();
        for i in 0..6 {
            let a = obstacle.points[i];
            let b = obstacle.points[i+1];
            println!("i is {}", i);
            assert_eq!(intersection_length(a.0,a.1, b.0,b.1, &obstacle.points, &obstacle.bounds), 0.0);
        }
        let a = obstacle.points[7];
        let b = obstacle.points[0];
        assert_eq!(intersection_length(a.0,a.1, b.0,b.1, &obstacle.points, &obstacle.bounds), 0.0);
    }
}
